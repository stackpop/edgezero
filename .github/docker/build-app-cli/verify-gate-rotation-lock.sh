#!/usr/bin/env bash
# shellcheck disable=SC2016 # jq programs must remain literal single-quoted expressions.
set +x +a
set -euo pipefail

TOKEN=${GITHUB_TOKEN-}
export -n TOKEN
unset GITHUB_TOKEN GH_TOKEN

export LC_ALL=C
export BASH_ENV=
export ENV=
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_NO_REPLACE_OBJECTS=1
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY
unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_CEILING_DIRECTORIES
unset GIT_REPLACE_REF_BASE

readonly API_BASE=https://api.github.com/repos/stackpop/edgezero
readonly API_VERSION=2026-03-10
readonly ROTATION_WORKFLOW=rotate-build-container-gate.yml
readonly ROTATION_PATH=.github/workflows/rotate-build-container-gate.yml@main
readonly ROTATION_ENVIRONMENT=build-container-gate-rotation-lock
readonly COMMENT_PREFIX='edgezero-gate-rotation-v1 '
readonly POLICY_PREFIX='edgezero-gate-rotation-policy-v1 '
readonly GATE_MANIFEST=.github/docker/build-app-cli/gate-paths.txt
readonly HELPER_PATH=.github/docker/build-app-cli/verify-gate-rotation-lock.sh
readonly U64_MAX=18446744073709551615
readonly U32_MAX=4294967295
readonly MAX_HISTORY_ITEMS=10000
readonly MAX_PAGES=100
readonly MAX_RECORD_BYTES=16384
readonly MAX_MANIFEST_BYTES=65536

usage() {
  printf '%s\n' \
    "usage: verify-gate-rotation-lock.sh waiting \\" \
    "  --gate-root <canonical-old-G-root> \\" \
    "  --old-gate-sha <G> \\" \
    "  --dispatch-sha <Q_d> \\" \
    "  --run-id <u64> \\" \
    "  --run-attempt <u32> \\" \
    '  --run-actor-login <login>' \
    '' \
    "usage: verify-gate-rotation-lock.sh publisher \\" \
    "  --gate-root <canonical-G-root> \\" \
    "  --gate-sha <G> \\" \
    "  --source-revision <S> \\" \
    '  --publisher-prerequisite-json <canonical-record>' >&2
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

is_login() {
  [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,38}(\[bot\])?$ ]]
}

is_digest() {
  [[ "$1" =~ ^sha256:[0-9a-f]{64}$ ]]
}

is_positive_decimal_at_most() {
  local value=$1 maximum=$2
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || return 1
  ((${#value} < ${#maximum})) && return 0
  ((${#value} == ${#maximum})) && [[ "$value" < "$maximum" || "$value" == "$maximum" ]]
}

is_nonnegative_decimal_at_most() {
  local value=$1 maximum=$2
  [[ "$value" == 0 ]] && return 0
  is_positive_decimal_at_most "$value" "$maximum"
}

decimal_greater_than() {
  local left=$1 right=$2
  ((${#left} > ${#right})) && return 0
  ((${#left} == ${#right})) && [[ "$left" > "$right" ]]
}

is_beneath() {
  [[ "$1" == "$2" || "$1" == "$2/"* ]]
}

safe_jq() {
  env -i PATH="$PATH" LC_ALL=C jq "$@"
}

safe_date_epoch() {
  env -i PATH="$PATH" LC_ALL=C date -u '+%s'
}

repo_git() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_NO_REPLACE_OBJECTS=1 GIT_OPTIONAL_LOCKS=0 \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null \
      -C "$GATE_ROOT" "$@"
}

canonical_root() {
  local supplied=$1 canonical
  [[ "$supplied" == /* && -d "$supplied" && ! -L "$supplied" ]] ||
    die "gate root must be an absolute, non-symlink directory"
  canonical=$(cd -- "$supplied" && pwd -P) || die "cannot resolve gate root"
  [[ "$canonical" == "$supplied" ]] || die "gate root must already be canonical"
  printf '%s\n' "$canonical"
}

require_checkout() {
  local expected=$1 top git_directory common_directory replacements shallow actual
  local sparse status gitlinks partial promisor
  [[ "$(repo_git rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] ||
    die "gate root is not a Git worktree"
  top=$(repo_git rev-parse --show-toplevel 2>/dev/null) || die "cannot resolve gate top level"
  [[ "$top" == "$GATE_ROOT" ]] || die "gate root must be the exact repository top level"
  git_directory=$(repo_git rev-parse --absolute-git-dir 2>/dev/null) ||
    die "cannot resolve gate Git directory"
  common_directory=$(repo_git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) ||
    die "cannot resolve gate common Git directory"
  [[ ! -e "$git_directory/info/grafts" && ! -L "$git_directory/info/grafts" &&
    ! -e "$common_directory/info/grafts" && ! -L "$common_directory/info/grafts" ]] ||
    die "gate checkout cannot contain legacy grafts"
  [[ ! -e "$git_directory/objects/info/alternates" &&
    ! -L "$git_directory/objects/info/alternates" &&
    ! -e "$common_directory/objects/info/alternates" &&
    ! -L "$common_directory/objects/info/alternates" ]] ||
    die "gate checkout cannot use object alternates"
  replacements=$(repo_git for-each-ref --format='%(refname)' refs/replace/ 2>/dev/null) ||
    die "cannot inspect gate replacement refs"
  [[ -z "$replacements" ]] || die "gate checkout cannot contain replacement refs"
  shallow=$(repo_git rev-parse --is-shallow-repository 2>/dev/null) ||
    die "cannot inspect gate history depth"
  [[ "$shallow" == false ]] || die "gate checkout must contain full history"
  partial=$(repo_git config --get extensions.partialclone 2>/dev/null || true)
  [[ -z "$partial" ]] || die "gate checkout cannot be partial"
  promisor=$(repo_git config --get-regexp '^remote\..*\.promisor$' 2>/dev/null || true)
  [[ -z "$promisor" ]] || die "gate checkout cannot use a promisor remote"
  actual=$(repo_git rev-parse --verify HEAD 2>/dev/null) || die "gate checkout HEAD is absent"
  [[ "$actual" == "$expected" ]] || die "gate checkout HEAD differs from its supplied SHA"
  if repo_git symbolic-ref -q HEAD >/dev/null 2>&1; then
    die "gate checkout must be detached"
  fi
  sparse=$(repo_git config --bool core.sparseCheckout 2>/dev/null || true)
  [[ "$sparse" != true ]] || die "gate checkout cannot be sparse"
  status=$(repo_git status --porcelain=v1 --untracked-files=all --ignore-submodules=none) ||
    die "cannot inspect gate checkout status"
  [[ -z "$status" ]] || die "gate checkout must be clean"
  gitlinks=$(repo_git ls-tree -r "$expected" 2>/dev/null | awk '$1 == "160000" { print; exit }') ||
    die "cannot inspect gate submodule state"
  [[ -z "$gitlinks" ]] || die "gate checkout cannot contain submodule state"
}

require_input_file() {
  local path=$1 parent canonical_parent expected size
  [[ "$path" == /* && -f "$path" && ! -L "$path" ]] ||
    die "publisher prerequisite must be an absolute regular non-symlink file"
  parent=${path%/*}
  [[ -n "$parent" ]] || parent=/
  canonical_parent=$(cd -- "$parent" && pwd -P) || die "cannot resolve prerequisite parent"
  [[ "$canonical_parent" == "$parent" ]] || die "prerequisite path must already be canonical"
  if [[ "$parent" == / ]]; then expected="/${path##*/}"; else expected="$parent/${path##*/}"; fi
  [[ "$expected" == "$path" ]] || die "prerequisite path must already be canonical"
  ! is_beneath "$path" "$GATE_ROOT" || die "publisher prerequisite must be outside the gate repository"
  size=$(wc -c <"$path" | tr -d '[:space:]') || die "cannot size publisher prerequisite"
  [[ "$size" =~ ^[0-9]+$ && "$size" -gt 0 && "$size" -le "$MAX_RECORD_BYTES" ]] ||
    die "publisher prerequisite is empty or oversized"
}

temporary_files=()
remove_temporary_files() {
  local path
  for path in ${temporary_files[@]+"${temporary_files[@]}"}; do
    [[ -z "$path" ]] || rm -f -- "$path" 2>/dev/null || true
  done
  [[ -z "${TEMP_ROOT:-}" ]] || rm -rf -- "$TEMP_ROOT" 2>/dev/null || true
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
  path=$(mktemp "$TEMP_ROOT/.edgezero-rotation-$label.XXXXXX" 2>/dev/null) ||
    tool_die "cannot create a temporary file"
  temporary_files+=("$path")
  printf -v "$result_name" '%s' "$path"
}

hash_file() {
  local path=$1 output
  output=$(env -i PATH="$PATH" LC_ALL=C sha256sum "$path" 2>/dev/null) ||
    tool_die "cannot hash trusted bytes"
  [[ "$output" =~ ^[0-9a-f]{64}[[:space:]] ]] || tool_die "sha256sum returned malformed output"
  printf '%s' "${output%%[[:space:]]*}"
}

valid_content_type() {
  [[ "$1" == application/json || "$1" == 'application/json; charset=utf-8' ]]
}

curl_get() {
  local label=$1 url=$2 result_name=$3 link_name=$4 response_body response_metadata line response_link
  local -a metadata_lines=()
  new_temporary_file response_body body
  new_temporary_file response_metadata metadata
  if ! printf '%s\n' \
    'header = "Accept: application/vnd.github+json"' \
    'header = "X-GitHub-Api-Version: 2026-03-10"' \
    'header = "User-Agent: edgezero-build-container-gate/1"' \
    "header = \"Authorization: Bearer $TOKEN\"" |
    env -i PATH="$PATH" LC_ALL=C curl \
      --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
      --request GET --config - --output "$response_body" \
      --write-out '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}\nlink=%header{link}' \
      "$url" >"$response_metadata" 2>/dev/null; then
    die "$label request failed"
  fi
  while IFS= read -r line || [[ -n "$line" ]]; do metadata_lines+=("$line"); done <"$response_metadata"
  [[ "${#metadata_lines[@]}" -eq 4 ]] || die "$label response metadata is malformed"
  [[ "${metadata_lines[0]}" == 200 ]] || die "$label response did not return HTTP 200"
  [[ "${metadata_lines[1]}" == "$API_VERSION" ]] ||
    die "$label response selected an unexpected API version"
  valid_content_type "${metadata_lines[2]}" || die "$label response has an unexpected content type"
  [[ "${metadata_lines[3]}" == link=* ]] || die "$label Link metadata is malformed"
  response_link=${metadata_lines[3]#link=}
  safe_jq -e -s 'length == 1' "$response_body" >/dev/null 2>&1 ||
    die "$label response is not exactly one JSON value"
  printf -v "$result_name" '%s' "$response_body"
  printf -v "$link_name" '%s' "$response_link"
}

write_number_stream() {
  local body=$1 output=$2 paths numbers path_count number_count
  new_temporary_file paths number-paths
  new_temporary_file numbers number-tokens
  safe_jq -rc --stream \
    'select(length == 2 and (.[1] | type) == "number") | (.[0] | @json)' \
    "$body" >"$paths" 2>/dev/null || die "cannot enumerate JSON number paths"
  awk '
    BEGIN { in_string = 0; escaped = 0 }
    {
      s = $0
      for (i = 1; i <= length(s); i++) {
        c = substr(s, i, 1)
        if (in_string) {
          if (escaped) { escaped = 0; continue }
          if (c == "\\") { escaped = 1; continue }
          if (c == "\"") in_string = 0
          continue
        }
        if (c == "\"") { in_string = 1; continue }
        if (c ~ /[-0-9]/) {
          token = c
          for (j = i + 1; j <= length(s); j++) {
            q = substr(s, j, 1)
            if (q !~ /[0-9eE+.-]/) break
            token = token q
          }
          print token
          i = j - 1
        }
      }
    }
    END { if (in_string || escaped) exit 1 }
  ' "$body" >"$numbers" || die "cannot preserve raw JSON numbers"
  path_count=$(wc -l <"$paths" | tr -d '[:space:]')
  number_count=$(wc -l <"$numbers" | tr -d '[:space:]')
  [[ "$path_count" == "$number_count" ]] || die "JSON numeric token stream is ambiguous"
  paste "$paths" "$numbers" >"$output" || tool_die "cannot pair JSON number tokens"
}

number_at_path() {
  local stream=$1 path=$2 result_name=$3 matches count value
  new_temporary_file matches number-match
  awk -F '\t' -v wanted="$path" '$1 == wanted { print $2 }' "$stream" >"$matches" ||
    die "cannot select a JSON number"
  count=$(wc -l <"$matches" | tr -d '[:space:]')
  [[ "$count" == 1 ]] || die "JSON number is missing or duplicated at $path"
  value=$(<"$matches")
  printf -v "$result_name" '%s' "$value"
}

validate_time() {
  local value=$1 result_name=$2 epoch round_trip
  [[ "$value" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] || return 1
  epoch=$(safe_jq -nr --arg value "$value" '$value | fromdateiso8601' 2>/dev/null) || return 1
  [[ "$epoch" =~ ^[0-9]+$ ]] || return 1
  round_trip=$(safe_jq -nr --argjson epoch "$epoch" \
    '$epoch | strftime("%Y-%m-%dT%H:%M:%SZ")' 2>/dev/null) || return 1
  [[ "$round_trip" == "$value" ]] || return 1
  printf -v "$result_name" '%s' "$epoch"
}

commit_exists() {
  [[ "$(repo_git cat-file -t "$1" 2>/dev/null)" == commit ]]
}

valid_manifest_path() {
  local path=$1
  [[ -n "$path" && "$path" =~ ^[A-Za-z0-9._/+-]+$ && "$path" != /* &&
    "$path" != -* && "$path" != */ && "$path" != . && "$path" != .. &&
    "$path" != ../* && "$path" != */../* && "$path" != */.. &&
    "$path" != */./* && "$path" != */. && "$path" != *//* && "$path" != *\\* ]]
}

tree_entry_or_absent() {
  local revision=$1 path=$2 result_name=$3 tree_entry metadata recorded mode type object
  tree_entry=$(repo_git ls-tree "$revision" -- "$path" 2>/dev/null) ||
    die "cannot inspect manifested path $path"
  if [[ -z "$tree_entry" ]]; then
    printf -v "$result_name" '%s' ''
    return
  fi
  [[ "$tree_entry" != *$'\n'* && "$tree_entry" == *$'\t'* ]] ||
    die "manifested path is ambiguous: $path"
  metadata=${tree_entry%%$'\t'*}
  recorded=${tree_entry#*$'\t'}
  read -r mode type object <<<"$metadata"
  [[ "$recorded" == "$path" && "$type" == blob &&
    ("$mode" == 100644 || "$mode" == 100755) && "$object" =~ ^[0-9a-f]{40,64}$ ]] ||
    die "manifested path is not a regular Git blob: $path"
  printf -v "$result_name" '%s' "$tree_entry"
}

extract_manifest() {
  local revision=$1 output=$2 entry size last previous current
  tree_entry_or_absent "$revision" "$GATE_MANIFEST" entry
  [[ -n "$entry" ]] || die "gate manifest is absent at $revision"
  repo_git show "$revision:$GATE_MANIFEST" >"$output" 2>/dev/null ||
    die "cannot read gate manifest at $revision"
  size=$(wc -c <"$output" | tr -d '[:space:]') || die "cannot size gate manifest"
  [[ "$size" =~ ^[0-9]+$ && "$size" -gt 0 && "$size" -le "$MAX_MANIFEST_BYTES" ]] ||
    die "gate manifest is empty or oversized"
  last=$(tail -c 1 "$output" | od -An -tx1 | tr -d '[:space:]') ||
    die "cannot inspect gate manifest terminator"
  [[ "$last" == 0a ]] || die "gate manifest must be LF-terminated"
  previous=
  while IFS= read -r current; do
    valid_manifest_path "$current" || die "gate manifest contains an invalid path"
    [[ -z "$previous" || "$previous" < "$current" ]] ||
      die "gate manifest must be byte-sorted and unique"
    previous=$current
  done <"$output"
  grep -Fqx -e "$GATE_MANIFEST" "$output" || die "gate manifest must contain itself"
  grep -Fqx -e "$HELPER_PATH" "$output" || die "gate manifest must contain the rotation helper"
}

compare_final_gate_tree() {
  local old_gate=$1 new_gate=$2 final_gate=$3 final_head=$4
  local old_manifest new_manifest union path expected actual
  new_temporary_file old_manifest old-manifest
  new_temporary_file new_manifest new-manifest
  new_temporary_file union manifest-union
  extract_manifest "$old_gate" "$old_manifest"
  extract_manifest "$new_gate" "$new_manifest"
  { cat "$old_manifest"; cat "$new_manifest"; } | LC_ALL=C sort -u >"$union"
  while IFS= read -r path; do
    tree_entry_or_absent "$final_gate" "$path" expected
    tree_entry_or_absent "$final_head" "$path" actual
    [[ "$actual" == "$expected" ]] || die "final head differs from the final gate at $path"
  done <"$union"
}

compare_active_gate_tree() {
  local active_gate=$1 main_sha=$2 manifest path expected actual
  new_temporary_file manifest active-gate-manifest
  extract_manifest "$active_gate" "$manifest"
  while IFS= read -r path; do
    tree_entry_or_absent "$active_gate" "$path" expected
    tree_entry_or_absent "$main_sha" "$path" actual
    [[ "$actual" == "$expected" ]] ||
      die "protected main differs from the active gate at $path"
  done <"$manifest"
}

parse_prerequisite() {
  local raw values extra history_state canonical issue_id expected_url
  safe_jq -e '
    type == "object"
    and (keys == ["evidence-sha256", "evidence-url", "gate-sha",
      "previous-value-sha256", "rotation-history", "schema-version",
      "source-pr", "source-revision"])
    and (."evidence-sha256" | type) == "string"
    and (."evidence-url" | type) == "string"
    and (."gate-sha" | type) == "string"
    and (."previous-value-sha256" | type) == "string"
    and (."rotation-history" | type) == "object"
    and ."schema-version" == 2
    and (."source-pr" | type) == "string"
    and (."source-revision" | type) == "string"
  ' "$PREREQUISITE_JSON" >/dev/null 2>&1 ||
    die "publisher prerequisite is not a schema-version-2 release-bound record"
  values=$(safe_jq -er '[."evidence-sha256", ."evidence-url", ."gate-sha",
    ."previous-value-sha256", ."source-pr", ."source-revision",
    ."rotation-history".state] | @tsv' "$PREREQUISITE_JSON" 2>/dev/null) ||
    die "cannot parse publisher prerequisite"
  IFS=$'\t' read -r PR_OUTER_EVIDENCE PR_EVIDENCE_URL PR_GATE PR_PREVIOUS PR_SOURCE_PR \
    PR_SOURCE history_state extra <<<"$values"
  [[ -z "${extra:-}" ]] || die "publisher prerequisite fields are malformed"
  is_digest "$PR_OUTER_EVIDENCE" || die "publisher prerequisite evidence digest is malformed"
  is_digest "$PR_PREVIOUS" || die "publisher prerequisite predecessor digest is malformed"
  is_sha "$PR_GATE" || die "publisher prerequisite gate SHA is malformed"
  is_sha "$PR_SOURCE" || die "publisher prerequisite source revision is malformed"
  is_positive_decimal_at_most "$PR_SOURCE_PR" "$U64_MAX" ||
    die "publisher prerequisite source PR is malformed"
  expected_url="https://github.com/stackpop/edgezero/pull/$PR_SOURCE_PR#issuecomment-"
  [[ "$PR_EVIDENCE_URL" == "$expected_url"* ]] ||
    die "publisher prerequisite evidence URL has the wrong repository or PR"
  issue_id=${PR_EVIDENCE_URL#"$expected_url"}
  is_positive_decimal_at_most "$issue_id" "$U64_MAX" ||
    die "publisher prerequisite evidence URL comment id is malformed"
  [[ "$PR_GATE" == "$GATE_SHA" ]] || die "publisher prerequisite gate differs from active gate"
  [[ "$PR_SOURCE" == "$SOURCE_REVISION" ]] ||
    die "publisher prerequisite source differs from tag source"

  case "$history_state" in
    bootstrap-no-rotation)
      safe_jq -e '."rotation-history" == {"state":"bootstrap-no-rotation"}' \
        "$PREREQUISITE_JSON" >/dev/null 2>&1 ||
        die "bootstrap rotation history is not exact"
      PR_HISTORY_STATE=bootstrap
      canonical="{\"evidence-sha256\":\"$PR_OUTER_EVIDENCE\",\"evidence-url\":\"$PR_EVIDENCE_URL\",\"gate-sha\":\"$PR_GATE\",\"previous-value-sha256\":\"$PR_PREVIOUS\",\"rotation-history\":{\"state\":\"bootstrap-no-rotation\"},\"schema-version\":2,\"source-pr\":\"$PR_SOURCE_PR\",\"source-revision\":\"$PR_SOURCE\"}"
      ;;
    verified)
      safe_jq -e '
        (."rotation-history" | keys) == ["created-at", "evidence-sha256",
          "history-sha256", "run-attempt", "run-id", "run-number", "state"]
        and all(."rotation-history"[]; type == "string")
      ' "$PREREQUISITE_JSON" >/dev/null 2>&1 ||
        die "verified rotation history shape is not exact"
      values=$(safe_jq -er '[."rotation-history"."created-at",
        ."rotation-history"."evidence-sha256", ."rotation-history"."history-sha256",
        ."rotation-history"."run-attempt", ."rotation-history"."run-id",
        ."rotation-history"."run-number"] | @tsv' "$PREREQUISITE_JSON" 2>/dev/null) ||
        die "cannot parse verified rotation history"
      IFS=$'\t' read -r PR_CREATED PR_RECEIPT_DIGEST PR_HISTORY_DIGEST PR_RUN_ATTEMPT \
        PR_RUN_ID PR_RUN_NUMBER extra <<<"$values"
      [[ -z "${extra:-}" ]] || die "verified rotation history fields are malformed"
      validate_time "$PR_CREATED" ignored_epoch || die "rotation history creation time is malformed"
      is_digest "$PR_RECEIPT_DIGEST" || die "rotation receipt digest is malformed"
      is_digest "$PR_HISTORY_DIGEST" || die "complete history digest is malformed"
      is_positive_decimal_at_most "$PR_RUN_ATTEMPT" "$U32_MAX" ||
        die "rotation history attempt is malformed"
      is_positive_decimal_at_most "$PR_RUN_ID" "$U64_MAX" ||
        die "rotation history run id is malformed"
      is_positive_decimal_at_most "$PR_RUN_NUMBER" "$U64_MAX" ||
        die "rotation history run number is malformed"
      PR_HISTORY_STATE=verified
      canonical="{\"evidence-sha256\":\"$PR_OUTER_EVIDENCE\",\"evidence-url\":\"$PR_EVIDENCE_URL\",\"gate-sha\":\"$PR_GATE\",\"previous-value-sha256\":\"$PR_PREVIOUS\",\"rotation-history\":{\"created-at\":\"$PR_CREATED\",\"evidence-sha256\":\"$PR_RECEIPT_DIGEST\",\"history-sha256\":\"$PR_HISTORY_DIGEST\",\"run-attempt\":\"$PR_RUN_ATTEMPT\",\"run-id\":\"$PR_RUN_ID\",\"run-number\":\"$PR_RUN_NUMBER\",\"state\":\"verified\"},\"schema-version\":2,\"source-pr\":\"$PR_SOURCE_PR\",\"source-revision\":\"$PR_SOURCE\"}"
      ;;
    *) die "publisher prerequisite rotation history state is invalid" ;;
  esac
  raw=$(<"$PREREQUISITE_JSON")
  [[ "${#raw}" -eq "$(wc -c <"$PREREQUISITE_JSON" | tr -d '[:space:]')" ]] ||
    die "publisher prerequisite contains trailing or non-text bytes"
  [[ "$raw" == "$canonical" ]] || die "publisher prerequisite bytes are not exact JCS"
}

list_url() {
  local kind=$1 page=$2
  case "$kind" in
    history)
      printf '%s/actions/workflows/%s/runs?event=workflow_dispatch&per_page=100&page=%s' \
        "$API_BASE" "$ROTATION_WORKFLOW" "$page"
      ;;
    jobs)
      printf '%s/actions/runs/%s/attempts/%s/jobs?per_page=100&page=%s' \
        "$API_BASE" "$SELECTED_ID" "$SELECTED_ATTEMPT" "$page"
      ;;
    *) die "internal list kind is invalid" ;;
  esac
}

validate_link_header() {
  local kind=$1 link=$2 current=$3 total=$4 needs_next=$5
  local remaining part url suffix relation target expected last seen=' '
  [[ -n "$link" ]] || { [[ "$needs_next" == false ]]; return; }
  remaining=$link
  while :; do
    if [[ "$remaining" == *,* ]]; then
      part=${remaining%%,*}
      remaining=${remaining#*,}
      [[ "$remaining" == ' '* ]] || die "$kind Link relations are not canonically separated"
      remaining=${remaining# }
    else
      part=$remaining
      remaining=
    fi
    [[ "$part" == '<'*'>; rel="'*'"' ]] || die "$kind Link relation is malformed"
    url=${part#<}
    url=${url%%>*}
    suffix=${part#*>}
    case "$suffix" in
      '; rel="first"') relation=first ;;
      '; rel="prev"') relation=prev ;;
      '; rel="next"') relation=next ;;
      '; rel="last"') relation=last ;;
      *) die "$kind Link relation is unsupported" ;;
    esac
    [[ "$seen" != *" $relation "* ]] || die "$kind Link relation is duplicated"
    seen+="$relation "
    last=$(((total + 99) / 100))
    ((last > 0)) || last=1
    case "$relation" in
      first) target=1 ;;
      prev)
        ((current > 1)) || die "$kind Link has impossible prev relation"
        target=$((current - 1))
        ;;
      next)
        [[ "$needs_next" == true ]] || die "$kind Link has an unexpected next relation"
        target=$((current + 1))
        ;;
      last) target=$last ;;
    esac
    expected=$(list_url "$kind" "$target")
    [[ "$url" == "$expected" ]] || die "$kind Link target differs from the closed query contract"
    [[ -n "$remaining" ]] || break
  done
  if [[ "$needs_next" == true ]]; then
    [[ "$seen" == *' next '* ]] || die "$kind pagination is missing its next relation"
  fi
}

enumerate_history() {
  local snapshot=$1 page=1 accumulated=0 declared_total='' raw_total body link stream
  local page_length i id attempt number created created_epoch round_trip padded rows sorted
  local seen_ids seen_numbers need_next digest separator row number_path raw index field date_rows
  local -a page_ids=() page_attempts=() page_numbers=() page_created=()
  new_temporary_file rows history-rows
  new_temporary_file sorted history-sorted
  new_temporary_file seen_ids history-ids
  new_temporary_file seen_numbers history-numbers
  : >"$rows"; : >"$seen_ids"; : >"$seen_numbers"
  while :; do
    ((page <= MAX_PAGES)) || die "rotation history exceeds the page bound"
    curl_get "rotation history page $page" "$(list_url history "$page")" body link
    safe_jq -e 'type == "object" and (.workflow_runs | type) == "array"' "$body" \
      >/dev/null 2>&1 || die "rotation history page shape is malformed"
    new_temporary_file stream history-numbers
    write_number_stream "$body" "$stream"
    raw_total=
    page_ids=()
    page_attempts=()
    page_numbers=()
    while IFS=$'\t' read -r number_path raw; do
      case "$number_path" in
        '["total_count"]')
          [[ -z "$raw_total" ]] || die "rotation history total count is duplicated"
          raw_total=$raw
          ;;
        '["workflow_runs",'*',"id"]'|'["workflow_runs",'*',"run_attempt"]'|'["workflow_runs",'*',"run_number"]')
          if [[ "$number_path" =~ ^\[\"workflow_runs\",([0-9]+),\"(id|run_attempt|run_number)\"\]$ ]]; then
            index=${BASH_REMATCH[1]}
            field=${BASH_REMATCH[2]}
          else
            die "rotation history numeric path is malformed"
          fi
          case "$field" in
            id)
              [[ -z "${page_ids[index]:-}" ]] || die "rotation run id is duplicated"
              page_ids[index]=$raw
              ;;
            run_attempt)
              [[ -z "${page_attempts[index]:-}" ]] || die "rotation run attempt is duplicated"
              page_attempts[index]=$raw
              ;;
            run_number)
              [[ -z "${page_numbers[index]:-}" ]] || die "rotation run number is duplicated"
              page_numbers[index]=$raw
              ;;
          esac
          ;;
      esac
    done <"$stream"
    [[ -n "$raw_total" ]] || die "rotation history total count is missing"
    is_nonnegative_decimal_at_most "$raw_total" "$MAX_HISTORY_ITEMS" ||
      die "rotation history total count is malformed or too large"
    if [[ -z "$declared_total" ]]; then declared_total=$raw_total; else
      [[ "$declared_total" == "$raw_total" ]] || die "rotation history total count changed between pages"
    fi
    page_length=$(safe_jq -er '.workflow_runs | length' "$body" 2>/dev/null) ||
      die "cannot count rotation history page"
    [[ "$page_length" =~ ^[0-9]+$ && "$page_length" -le 100 ]] ||
      die "rotation history page exceeds 100 items"
    if ((page == MAX_PAGES && page_length == 100)); then
      die "rotation history page bound ended on a full page"
    fi
    new_temporary_file date_rows history-dates
    safe_jq -r '
      .workflow_runs[]
      | .created_at as $created
      | if (($created | type) == "string")
          and ($created | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$"))
        then ($created | fromdateiso8601) as $epoch
        | [$created, ($epoch | tostring), ($epoch | strftime("%Y-%m-%dT%H:%M:%SZ"))] | @tsv
        else error("malformed creation time")
        end
    ' "$body" >"$date_rows" 2>/dev/null || die "rotation run creation time is malformed"
    page_created=()
    i=0
    while IFS=$'\t' read -r created created_epoch round_trip; do
      [[ "$round_trip" == "$created" && "$created_epoch" =~ ^[0-9]+$ ]] ||
        die "rotation run creation time is malformed"
      page_created[i]="$created"$'\t'"$created_epoch"
      i=$((i + 1))
    done <"$date_rows"
    [[ "$i" -eq "$page_length" ]] || die "rotation run creation time count differs"
    for ((i = 0; i < page_length; i++)); do
      id=${page_ids[$i]:-}
      attempt=${page_attempts[$i]:-}
      number=${page_numbers[$i]:-}
      is_positive_decimal_at_most "$id" "$U64_MAX" || die "rotation run id is malformed"
      is_positive_decimal_at_most "$attempt" "$U32_MAX" || die "rotation run attempt is malformed"
      is_positive_decimal_at_most "$number" "$U64_MAX" || die "rotation run number is malformed"
      IFS=$'\t' read -r created created_epoch <<<"${page_created[$i]}"
      ((created_epoch <= NOW_EPOCH)) || die "rotation run creation time is in the future"
      ! grep -Fqx -e "$id" "$seen_ids" || die "rotation history contains a duplicate run id"
      ! grep -Fqx -e "$number" "$seen_numbers" || die "rotation history contains a duplicate run number"
      printf '%s\n' "$id" >>"$seen_ids"
      printf '%s\n' "$number" >>"$seen_numbers"
      padded=$(printf '%020s' "$number" | tr ' ' 0)
      printf '%s\t%s\t%s\t%s\t%s\n' "$padded" "$attempt" "$id" "$number" "$created" >>"$rows"
    done
    accumulated=$((accumulated + page_length))
    ((accumulated <= declared_total)) || die "rotation history contains more runs than total_count"
    if ((accumulated < declared_total)); then
      [[ "$page_length" -eq 100 ]] || die "rotation history is truncated before total_count"
      ((page < MAX_PAGES)) || die "rotation history is truncated at page 100"
      need_next=true
    else
      need_next=false
    fi
    validate_link_header history "$link" "$page" "$declared_total" "$need_next"
    [[ "$need_next" == true ]] || break
    page=$((page + 1))
  done
  [[ "$accumulated" == "$declared_total" ]] || die "rotation history count is incomplete"
  LC_ALL=C sort "$rows" >"$sorted" || tool_die "cannot sort rotation history"
  printf '[' >"$snapshot"
  separator=
  ENUM_SELECTED_ID=
  ENUM_SELECTED_ATTEMPT=
  ENUM_SELECTED_NUMBER=
  ENUM_SELECTED_CREATED=
  while IFS=$'\t' read -r _ attempt id number created; do
    row="{\"run-attempt\":\"$attempt\",\"run-id\":\"$id\",\"run-number\":\"$number\"}"
    printf '%s%s' "$separator" "$row" >>"$snapshot"
    separator=,
    ENUM_SELECTED_ID=$id
    ENUM_SELECTED_ATTEMPT=$attempt
    ENUM_SELECTED_NUMBER=$number
    ENUM_SELECTED_CREATED=$created
  done <"$sorted"
  printf ']' >>"$snapshot"
  ENUM_COUNT=$declared_total
  digest=$(hash_file "$snapshot")
  ENUM_DIGEST="sha256:$digest"
}

validate_run_detail() {
  local body=$1 mode=$2 expected_id=$3 expected_attempt=$4 expected_number=$5
  local expected_created=$6 result_name=$7 stream id attempt number values extra created_epoch
  local event path head branch actor repository head_repository status conclusion identity
  safe_jq -e '
    type == "object"
    and (.created_at | type) == "string"
    and (.event | type) == "string"
    and (.path | type) == "string"
    and (.head_sha | type) == "string"
    and (.head_branch | type) == "string"
    and (.actor.login | type) == "string"
    and (.repository.full_name | type) == "string"
    and (.head_repository.full_name | type) == "string"
    and (.status | type) == "string"
    and ((.conclusion == null) or ((.conclusion | type) == "string"))
  ' "$body" >/dev/null 2>&1 || die "rotation run detail shape is malformed"
  if [[ "$mode" == waiting ]]; then
    safe_jq -e '.status == "in_progress" and .conclusion == null' "$body" \
      >/dev/null 2>&1 || die "waiting run status or conclusion is invalid"
  else
    safe_jq -e '.status == "completed" and .conclusion == "success"' "$body" \
      >/dev/null 2>&1 || die "selected rotation run status or conclusion is invalid"
  fi
  new_temporary_file stream run-numbers
  write_number_stream "$body" "$stream"
  number_at_path "$stream" '["id"]' id
  number_at_path "$stream" '["run_attempt"]' attempt
  number_at_path "$stream" '["run_number"]' number
  is_positive_decimal_at_most "$id" "$U64_MAX" || die "rotation run detail id is malformed"
  is_positive_decimal_at_most "$attempt" "$U32_MAX" || die "rotation run detail attempt is malformed"
  is_positive_decimal_at_most "$number" "$U64_MAX" || die "rotation run detail number is malformed"
  [[ "$id" == "$expected_id" && "$attempt" == "$expected_attempt" ]] ||
    die "rotation run detail identifiers differ"
  [[ -z "$expected_number" || "$number" == "$expected_number" ]] ||
    die "rotation run detail run number differs"
  values=$(safe_jq -er '[.created_at, .event, .path, .head_sha, .head_branch,
    .actor.login, .repository.full_name, .head_repository.full_name, .status,
    (.conclusion // "")] | @tsv' "$body" 2>/dev/null) ||
    die "cannot parse rotation run detail fields"
  IFS=$'\t' read -r RUN_DETAIL_CREATED event path head branch actor repository head_repository \
    status conclusion extra <<<"$values"
  [[ -z "${extra:-}" ]] || die "rotation run detail fields are malformed"
  validate_time "$RUN_DETAIL_CREATED" created_epoch || die "rotation run detail creation time is malformed"
  ((created_epoch <= NOW_EPOCH)) || die "rotation run detail creation time is in the future"
  [[ -z "$expected_created" || "$RUN_DETAIL_CREATED" == "$expected_created" ]] ||
    die "rotation run detail creation time differs from history"
  [[ "$event" == workflow_dispatch ]] || die "rotation run event is not workflow_dispatch"
  [[ "$path" == "$ROTATION_PATH" ]] || die "rotation run path is not exact protected main"
  is_sha "$head" || die "rotation run dispatch head is malformed"
  [[ "$branch" == main ]] || die "rotation run branch is not main"
  is_login "$actor" || die "rotation run actor login is malformed"
  [[ "$repository" == stackpop/edgezero && "$head_repository" == stackpop/edgezero ]] ||
    die "rotation run repository identity differs"
  if [[ "$mode" == waiting ]]; then
    [[ "$head" == "$DISPATCH_SHA" ]] || die "waiting run dispatch SHA differs"
    [[ "$actor" == "$RUN_ACTOR_LOGIN" ]] || die "waiting run actor differs"
    [[ "$status" == in_progress && -z "$conclusion" ]] ||
      die "waiting run is not executing without a conclusion"
  else
    [[ "$status" == completed && "$conclusion" == success ]] ||
      die "selected rotation run is not completed successfully"
  fi
  RUN_DETAIL_HEAD=$head
  RUN_DETAIL_ACTOR=$actor
  RUN_DETAIL_NUMBER=$number
  identity="$id|$attempt|$number|$RUN_DETAIL_CREATED|$event|$path|$head|$branch|$actor|$repository|$head_repository|$status|$conclusion"
  printf -v "$result_name" '%s' "$identity"
}

enumerate_jobs() {
  local page=1 accumulated=0 declared_total='' raw_total body link stream page_length i
  local id attempt head status conclusion steps_length j name step_status step_conclusion completed
  local completed_epoch need_next seen_ids target_count=0
  new_temporary_file seen_ids job-ids
  : >"$seen_ids"
  while :; do
    ((page <= MAX_PAGES)) || die "rotation jobs exceed the page bound"
    curl_get "rotation jobs page $page" "$(list_url jobs "$page")" body link
    safe_jq -e 'type == "object" and (.jobs | type) == "array"' "$body" >/dev/null 2>&1 ||
      die "rotation jobs page shape is malformed"
    new_temporary_file stream job-numbers
    write_number_stream "$body" "$stream"
    number_at_path "$stream" '["total_count"]' raw_total
    is_nonnegative_decimal_at_most "$raw_total" "$MAX_HISTORY_ITEMS" ||
      die "rotation jobs total count is malformed or too large"
    if [[ -z "$declared_total" ]]; then declared_total=$raw_total; else
      [[ "$declared_total" == "$raw_total" ]] || die "rotation jobs total count changed between pages"
    fi
    page_length=$(safe_jq -er '.jobs | length' "$body" 2>/dev/null) || die "cannot count rotation jobs"
    [[ "$page_length" =~ ^[0-9]+$ && "$page_length" -le 100 ]] ||
      die "rotation jobs page exceeds 100 items"
    if ((page == MAX_PAGES && page_length == 100)); then
      die "rotation jobs page bound ended on a full page"
    fi
    for ((i = 0; i < page_length; i++)); do
      number_at_path "$stream" "[\"jobs\",$i,\"id\"]" id
      number_at_path "$stream" "[\"jobs\",$i,\"run_attempt\"]" attempt
      is_positive_decimal_at_most "$id" "$U64_MAX" || die "rotation job id is malformed"
      is_positive_decimal_at_most "$attempt" "$U32_MAX" || die "rotation job attempt is malformed"
      [[ "$attempt" == "$SELECTED_ATTEMPT" ]] || die "rotation job belongs to another attempt"
      ! grep -Fqx -e "$id" "$seen_ids" || die "rotation jobs contain a duplicate id"
      printf '%s\n' "$id" >>"$seen_ids"
      values=$(safe_jq -er --argjson index "$i" \
        '[.jobs[$index].head_sha, .jobs[$index].status, .jobs[$index].conclusion] | @tsv' \
        "$body" 2>/dev/null) || die "rotation job fields are malformed"
      IFS=$'\t' read -r head status conclusion extra <<<"$values"
      [[ -z "${extra:-}" ]] || die "rotation job fields are malformed"
      [[ "$head" == "$RUN_DETAIL_HEAD" && "$status" == completed && "$conclusion" == success ]] ||
        die "rotation job identity or result differs"
      steps_length=$(safe_jq -er --argjson index "$i" '.jobs[$index].steps | length' \
        "$body" 2>/dev/null) || die "rotation job steps are malformed"
      for ((j = 0; j < steps_length; j++)); do
        name=$(safe_jq -er --argjson i "$i" --argjson j "$j" \
          '.jobs[$i].steps[$j].name | select(type == "string")' "$body" 2>/dev/null) ||
          die "rotation job step name is malformed"
        [[ "$name" == assert-exact-rotation-context ]] || continue
        target_count=$((target_count + 1))
        values=$(safe_jq -er --argjson i "$i" --argjson j "$j" \
          '[.jobs[$i].steps[$j].status, .jobs[$i].steps[$j].conclusion,
            .jobs[$i].steps[$j].completed_at] | @tsv' "$body" 2>/dev/null) ||
          die "rotation context step fields are malformed"
        IFS=$'\t' read -r step_status step_conclusion completed extra <<<"$values"
        [[ -z "${extra:-}" && "$step_status" == completed && "$step_conclusion" == success ]] ||
          die "rotation context step did not complete successfully"
        validate_time "$completed" completed_epoch || die "rotation context completion time is malformed"
        STEP_COMPLETED_EPOCH=$completed_epoch
      done
    done
    accumulated=$((accumulated + page_length))
    ((accumulated <= declared_total)) || die "rotation jobs exceed total_count"
    if ((accumulated < declared_total)); then
      [[ "$page_length" -eq 100 ]] || die "rotation jobs are truncated before total_count"
      ((page < MAX_PAGES)) || die "rotation jobs are truncated at page 100"
      need_next=true
    else
      need_next=false
    fi
    validate_link_header jobs "$link" "$page" "$declared_total" "$need_next"
    [[ "$need_next" == true ]] || break
    page=$((page + 1))
  done
  [[ "$accumulated" == "$declared_total" ]] || die "rotation jobs count is incomplete"
  [[ "$target_count" -eq 1 ]] || die "rotation jobs lack one unique context step"
}

parse_receipt_comment() {
  local comment=$1 reviewer=$2 bound_epoch=$3 run_created=$4
  local first second first_json second_json values extra canonical_first canonical_second
  local evidence head_one id_one new_gate old_gate result reviewed
  local audited dispatch gate head_two attempt id_two policy release_state required
  local digest audited_epoch reviewed_epoch created_epoch
  [[ "$comment" == *$'\n'* ]] || die "rotation approval comment is not exactly two lines"
  first=${comment%%$'\n'*}
  second=${comment#*$'\n'}
  [[ "$second" != *$'\n'* && "$first" == "$COMMENT_PREFIX"* && "$second" == "$POLICY_PREFIX"* ]] ||
    die "rotation approval comment is not canonical two-line protocol"
  new_temporary_file first_json receipt
  new_temporary_file second_json policy-receipt
  printf '%s' "${first#"$COMMENT_PREFIX"}" >"$first_json"
  printf '%s' "${second#"$POLICY_PREFIX"}" >"$second_json"
  safe_jq -e '
    type == "object"
    and (keys == ["evidence-sha256", "head-sha", "lock-run-id", "new-gate-sha",
      "old-gate-sha", "result", "reviewed-at"])
    and all(.[]; type == "string")
  ' "$first_json" >/dev/null 2>&1 || die "rotation approval evidence shape is malformed"
  safe_jq -e '
    type == "object"
    and (keys == ["audited-at", "dispatch-sha", "gate-sha", "head-sha",
      "lock-run-attempt", "lock-run-id", "policy-sha256", "release-state",
      "required-workflow-sha"])
    and all(.[]; type == "string")
  ' "$second_json" >/dev/null 2>&1 || die "rotation policy receipt shape is malformed"
  values=$(safe_jq -er '[."evidence-sha256", ."head-sha", ."lock-run-id",
    ."new-gate-sha", ."old-gate-sha", .result, ."reviewed-at"] | @tsv' \
    "$first_json" 2>/dev/null) || die "cannot parse rotation approval evidence"
  IFS=$'\t' read -r evidence head_one id_one new_gate old_gate result reviewed extra <<<"$values"
  [[ -z "${extra:-}" ]] || die "rotation approval evidence fields are malformed"
  values=$(safe_jq -er '[."audited-at", ."dispatch-sha", ."gate-sha", ."head-sha",
    ."lock-run-attempt", ."lock-run-id", ."policy-sha256", ."release-state",
    ."required-workflow-sha"] | @tsv' "$second_json" 2>/dev/null) ||
    die "cannot parse rotation policy receipt"
  IFS=$'\t' read -r audited dispatch gate head_two attempt id_two policy release_state required \
    extra <<<"$values"
  [[ -z "${extra:-}" ]] || die "rotation policy receipt fields are malformed"
  is_digest "$evidence" || die "rotation receipt evidence digest is malformed"
  is_digest "$policy" || die "rotation policy evidence digest is malformed"
  if ! {
    is_sha "$head_one" && is_sha "$head_two" && is_sha "$new_gate" && is_sha "$old_gate" &&
      is_sha "$dispatch" && is_sha "$gate" && is_sha "$required"
  }; then
    die "rotation receipt contains a malformed SHA"
  fi
  if ! {
    is_positive_decimal_at_most "$id_one" "$U64_MAX" &&
      is_positive_decimal_at_most "$id_two" "$U64_MAX"
  }; then
    die "rotation receipt run id is malformed"
  fi
  is_positive_decimal_at_most "$attempt" "$U32_MAX" || die "rotation receipt attempt is malformed"
  [[ "$id_one" == "$SELECTED_ID" && "$id_two" == "$SELECTED_ID" &&
    "$attempt" == "$SELECTED_ATTEMPT" ]] || die "rotation receipt run identity differs"
  [[ "$head_one" == "$head_two" ]] || die "rotation receipt final heads differ"
  [[ "$dispatch" == "$RUN_DETAIL_HEAD" ]] || die "rotation receipt dispatch SHA differs from run"
  [[ "$release_state" == enabled ]] || die "rotation receipt release state is not enabled"
  [[ "$new_gate" != "$old_gate" ]] || die "rotation receipt does not describe a gate change"
  case "$result" in
    activated) RECEIPT_FINAL_GATE=$new_gate ;;
    rolled-back) RECEIPT_FINAL_GATE=$old_gate ;;
    *) die "rotation result is invalid" ;;
  esac
  [[ "$gate" == "$RECEIPT_FINAL_GATE" && "$required" == "$RECEIPT_FINAL_GATE" ]] ||
    die "rotation receipt gate pointers differ from its result"
  if [[ "$MODE" == publisher ]]; then
    [[ "$RECEIPT_FINAL_GATE" == "$EXPECTED_FINAL_GATE" ]] ||
      die "rotation receipt final gate differs from active gate"
  fi
  if [[ "$MODE" == waiting ]]; then
    [[ "$old_gate" == "$OLD_GATE_SHA" ]] || die "rotation receipt old gate differs from waiting gate"
  fi
  digest=$(hash_file "$second_json")
  [[ "$evidence" == "sha256:$digest" ]] || die "rotation receipt evidence digest differs"
  canonical_first="{\"evidence-sha256\":\"$evidence\",\"head-sha\":\"$head_one\",\"lock-run-id\":\"$id_one\",\"new-gate-sha\":\"$new_gate\",\"old-gate-sha\":\"$old_gate\",\"result\":\"$result\",\"reviewed-at\":\"$reviewed\"}"
  canonical_second="{\"audited-at\":\"$audited\",\"dispatch-sha\":\"$dispatch\",\"gate-sha\":\"$gate\",\"head-sha\":\"$head_two\",\"lock-run-attempt\":\"$attempt\",\"lock-run-id\":\"$id_two\",\"policy-sha256\":\"$policy\",\"release-state\":\"$release_state\",\"required-workflow-sha\":\"$required\"}"
  [[ "$(<"$first_json")" == "$canonical_first" && "$(<"$second_json")" == "$canonical_second" ]] ||
    die "rotation approval comment bytes are not exact JCS"
  validate_time "$run_created" created_epoch || die "rotation run creation time is malformed"
  validate_time "$audited" audited_epoch || die "rotation audit time is malformed"
  validate_time "$reviewed" reviewed_epoch || die "rotation review time is malformed"
  ((created_epoch <= audited_epoch && audited_epoch <= reviewed_epoch && reviewed_epoch <= bound_epoch)) ||
    die "rotation receipt timestamps are not ordered"
  ((bound_epoch - audited_epoch <= 900 && bound_epoch - reviewed_epoch <= 900)) ||
    die "rotation receipt is stale"
  RECEIPT_EVIDENCE=$evidence
  RECEIPT_HEAD=$head_one
  RECEIPT_OLD_GATE=$old_gate
  RECEIPT_NEW_GATE=$new_gate
}

validate_approvals() {
  local body=$1 link=$2 bound_epoch=$3 run_created=$4 reviews item comment reviewer state
  local environments count=0
  [[ -z "$link" ]] || die "rotation approvals unexpectedly require pagination"
  safe_jq -e 'type == "array"' "$body" >/dev/null 2>&1 ||
    die "rotation approvals response is not an array"
  new_temporary_file reviews reviews
  safe_jq -j --arg first "$COMMENT_PREFIX" --arg second "$POLICY_PREFIX" '
    .[]
    | select(type == "object" and (.comment | type) == "string"
        and ((.comment | startswith($first)) or (.comment | startswith($second))))
    | @json, "\u0000"
  ' "$body" >"$reviews" 2>/dev/null || die "cannot enumerate rotation approval comments"
  while IFS= read -r -d '' item; do
    count=$((count + 1))
    comment=$(safe_jq -er '.comment | select(type == "string")' <<<"$item" 2>/dev/null) ||
      die "rotation approval comment is malformed"
    values=$(safe_jq -er '[.state, .user.login, (.environments | length),
      .environments[0].name] | @tsv' <<<"$item" 2>/dev/null) ||
      die "rotation approval metadata is malformed"
    IFS=$'\t' read -r state reviewer environments environment extra <<<"$values"
    [[ -z "${extra:-}" && "$state" == approved && "$environments" == 1 &&
      "$environment" == "$ROTATION_ENVIRONMENT" ]] ||
      die "rotation approval metadata is not exact"
    is_login "$reviewer" || die "rotation approval reviewer login is malformed"
    [[ "$reviewer" != "$RUN_DETAIL_ACTOR" ]] ||
      die "rotation approval reviewer must differ from the run actor"
    parse_receipt_comment "$comment" "$reviewer" "$bound_epoch" "$run_created"
  done <"$reviews"
  [[ "$count" -eq 1 ]] || die "rotation approvals lack one unique protocol review"
}

validate_main_ref() {
  local body=$1 link=$2 result_name=$3 values sha type ref extra
  [[ -z "$link" ]] || die "main ref response has an unexpected Link header"
  safe_jq -e 'type == "object" and (.ref | type) == "string"
    and (.object | type) == "object" and (.object.sha | type) == "string"
    and (.object.type | type) == "string"' "$body" >/dev/null 2>&1 ||
    die "main ref response shape is malformed"
  values=$(safe_jq -er '[.ref, .object.sha, .object.type] | @tsv' "$body" 2>/dev/null) ||
    die "cannot parse main ref response"
  IFS=$'\t' read -r ref sha type extra <<<"$values"
  [[ -z "${extra:-}" && "$ref" == refs/heads/main && "$type" == commit ]] ||
    die "main ref identity is not exact"
  is_sha "$sha" || die "main ref commit SHA is malformed"
  commit_exists "$sha" || die "main ref commit is unavailable locally"
  printf -v "$result_name" '%s' "$sha"
}

validate_publisher_main() {
  local body link
  curl_get "protected main ref" "$API_BASE/git/ref/heads/main" body link
  MAIN_SHA=
  validate_main_ref "$body" "$link" MAIN_SHA
  repo_git merge-base --is-ancestor "$SOURCE_REVISION" "$MAIN_SHA" 2>/dev/null ||
    die "release source is not an ancestor of protected main"
  compare_active_gate_tree "$GATE_SHA" "$MAIN_SHA"
}

MODE=${1:-}
case "$MODE" in waiting | publisher) shift ;; *) usage ;; esac

GATE_ROOT=
OLD_GATE_SHA=
DISPATCH_SHA=
RUN_ID=
RUN_ATTEMPT=
RUN_ACTOR_LOGIN=
GATE_SHA=
SOURCE_REVISION=
PREREQUISITE_JSON=
seen_flags=' '

while (($#)); do
  (($# >= 2)) || usage
  flag=$1
  value=$2
  shift 2
  case "$MODE:$flag" in
    waiting:--gate-root|waiting:--old-gate-sha|waiting:--dispatch-sha|\
    waiting:--run-id|waiting:--run-attempt|waiting:--run-actor-login|\
    publisher:--gate-root|publisher:--gate-sha|publisher:--source-revision|\
    publisher:--publisher-prerequisite-json) ;;
    *) usage ;;
  esac
  [[ -n "$value" ]] || usage
  [[ "$seen_flags" != *" $flag "* ]] || usage
  seen_flags+="$flag "
  case "$flag" in
    --gate-root) GATE_ROOT=$value ;;
    --old-gate-sha) OLD_GATE_SHA=$value ;;
    --dispatch-sha) DISPATCH_SHA=$value ;;
    --run-id) RUN_ID=$value ;;
    --run-attempt) RUN_ATTEMPT=$value ;;
    --run-actor-login) RUN_ACTOR_LOGIN=$value ;;
    --gate-sha) GATE_SHA=$value ;;
    --source-revision) SOURCE_REVISION=$value ;;
    --publisher-prerequisite-json) PREREQUISITE_JSON=$value ;;
  esac
done

if [[ "$MODE" == waiting ]]; then
  required_flags='--gate-root --old-gate-sha --dispatch-sha --run-id --run-attempt --run-actor-login'
else
  required_flags='--gate-root --gate-sha --source-revision --publisher-prerequisite-json'
fi
for required in $required_flags; do [[ "$seen_flags" == *" $required "* ]] || usage; done

[[ -n "$TOKEN" ]] || die "GitHub token is absent"
[[ "$TOKEN" != *$'\n'* && "$TOKEN" != *$'\r'* && "$TOKEN" != *'"'* && "$TOKEN" != *\\* ]] ||
  die "GitHub token cannot be encoded safely"

for tool in env git jq curl awk sed sort mktemp cmp sha256sum date wc tail od grep paste tr rm cat dirname; do
  command -v "$tool" >/dev/null 2>&1 || tool_die "rotation verifier requires $tool"
done
safe_jq --version >/dev/null 2>&1 || tool_die "rotation verifier cannot execute jq"
NOW_EPOCH=$(safe_date_epoch 2>/dev/null) || tool_die "rotation verifier cannot read UTC time"
[[ "$NOW_EPOCH" =~ ^[0-9]+$ ]] || tool_die "rotation verifier received malformed UTC time"

if [[ "$MODE" == waiting ]]; then
  is_sha "$OLD_GATE_SHA" || die "old gate SHA is malformed"
  is_sha "$DISPATCH_SHA" || die "dispatch SHA is malformed"
  is_positive_decimal_at_most "$RUN_ID" "$U64_MAX" || die "run id is malformed"
  is_positive_decimal_at_most "$RUN_ATTEMPT" "$U32_MAX" || die "run attempt is malformed"
  is_login "$RUN_ACTOR_LOGIN" || die "run actor login is malformed"
  CHECKOUT_SHA=$OLD_GATE_SHA
else
  is_sha "$GATE_SHA" || die "gate SHA is malformed"
  is_sha "$SOURCE_REVISION" || die "source revision is malformed"
  CHECKOUT_SHA=$GATE_SHA
fi

GATE_ROOT=$(canonical_root "$GATE_ROOT")
require_checkout "$CHECKOUT_SHA"
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
[[ "$SCRIPT_DIR/verify-gate-rotation-lock.sh" == "$GATE_ROOT/$HELPER_PATH" ]] ||
  die "rotation verifier must execute from the canonical gate checkout"
TEMP_ROOT=$(mktemp -d /tmp/edgezero-rotation-lock.XXXXXX 2>/dev/null) ||
  tool_die "cannot create rotation verifier temporary directory"

if [[ "$MODE" == waiting ]]; then
  commit_exists "$DISPATCH_SHA" || die "dispatch commit is unavailable locally"
  repo_git merge-base --is-ancestor "$OLD_GATE_SHA" "$DISPATCH_SHA" 2>/dev/null ||
    die "old gate is not an ancestor of dispatch"
else
  require_input_file "$PREREQUISITE_JSON"
  parse_prerequisite
  commit_exists "$SOURCE_REVISION" || die "source revision is unavailable locally"
  repo_git merge-base --is-ancestor "$GATE_SHA" "$SOURCE_REVISION" 2>/dev/null ||
    die "source revision does not descend from active gate"
fi

if [[ "$MODE" == waiting ]]; then
  SELECTED_ID=$RUN_ID
  SELECTED_ATTEMPT=$RUN_ATTEMPT
  RUN_BODY=
  RUN_LINK=
  curl_get "waiting rotation run" "$API_BASE/actions/runs/$RUN_ID" RUN_BODY RUN_LINK
  [[ -z "$RUN_LINK" ]] || die "waiting run response has an unexpected Link header"
  validate_run_detail "$RUN_BODY" waiting "$RUN_ID" "$RUN_ATTEMPT" '' '' WAITING_IDENTITY
  SELECTED_NUMBER=$RUN_DETAIL_NUMBER
  EXPECTED_FINAL_GATE=$OLD_GATE_SHA
  APPROVAL_BODY=
  APPROVAL_LINK=
  curl_get "waiting rotation approvals" "$API_BASE/actions/runs/$RUN_ID/approvals" \
    APPROVAL_BODY APPROVAL_LINK
  validate_approvals "$APPROVAL_BODY" "$APPROVAL_LINK" "$NOW_EPOCH" "$RUN_DETAIL_CREATED"
  MAIN_BODY=
  MAIN_LINK=
  curl_get "protected main ref" "$API_BASE/git/ref/heads/main" MAIN_BODY MAIN_LINK
  validate_main_ref "$MAIN_BODY" "$MAIN_LINK" MAIN_SHA
  [[ "$MAIN_SHA" == "$RECEIPT_HEAD" ]] || die "waiting receipt head differs from current main"
  if ! {
    commit_exists "$RECEIPT_OLD_GATE" && commit_exists "$RECEIPT_NEW_GATE" &&
      commit_exists "$RECEIPT_HEAD"
  }; then
    die "rotation receipt commits are unavailable locally"
  fi
  repo_git merge-base --is-ancestor "$RUN_DETAIL_HEAD" "$RECEIPT_HEAD" 2>/dev/null ||
    die "dispatch is not an ancestor of final head"
  repo_git merge-base --is-ancestor "$RECEIPT_NEW_GATE" "$RECEIPT_HEAD" 2>/dev/null ||
    die "new gate is not an ancestor of final head"
  compare_final_gate_tree "$RECEIPT_OLD_GATE" "$RECEIPT_NEW_GATE" \
    "$RECEIPT_FINAL_GATE" "$RECEIPT_HEAD"
  exit 0
fi

HISTORY_ONE=
new_temporary_file HISTORY_ONE history-one
enumerate_history "$HISTORY_ONE"
FIRST_COUNT=$ENUM_COUNT
FIRST_DIGEST=$ENUM_DIGEST
FIRST_ID=$ENUM_SELECTED_ID
FIRST_ATTEMPT=$ENUM_SELECTED_ATTEMPT
FIRST_NUMBER=$ENUM_SELECTED_NUMBER
FIRST_CREATED=$ENUM_SELECTED_CREATED

if [[ "$FIRST_COUNT" == 0 ]]; then
  [[ "$PR_HISTORY_STATE" == bootstrap ]] || die "empty history requires bootstrap prerequisite"
  validate_publisher_main
  HISTORY_TWO=
  new_temporary_file HISTORY_TWO history-two
  enumerate_history "$HISTORY_TWO"
  [[ "$ENUM_COUNT" == 0 ]] || die "rotation appeared during bootstrap history verification"
  cmp -s "$HISTORY_ONE" "$HISTORY_TWO" || die "bootstrap history changed between snapshots"
  exit 0
fi

[[ "$PR_HISTORY_STATE" == verified ]] || die "nonempty history requires verified prerequisite"
[[ "$FIRST_ID" == "$PR_RUN_ID" && "$FIRST_ATTEMPT" == "$PR_RUN_ATTEMPT" &&
  "$FIRST_NUMBER" == "$PR_RUN_NUMBER" && "$FIRST_CREATED" == "$PR_CREATED" &&
  "$FIRST_DIGEST" == "$PR_HISTORY_DIGEST" ]] ||
  die "publisher prerequisite does not match current rotation history"

SELECTED_ID=$FIRST_ID
SELECTED_ATTEMPT=$FIRST_ATTEMPT
SELECTED_NUMBER=$FIRST_NUMBER
RUN_BODY=
RUN_LINK=
curl_get "selected rotation run" "$API_BASE/actions/runs/$SELECTED_ID" RUN_BODY RUN_LINK
[[ -z "$RUN_LINK" ]] || die "selected rotation run has an unexpected Link header"
validate_run_detail "$RUN_BODY" publisher "$SELECTED_ID" "$SELECTED_ATTEMPT" \
  "$SELECTED_NUMBER" "$FIRST_CREATED" FIRST_RUN_IDENTITY

enumerate_jobs

EXPECTED_FINAL_GATE=$GATE_SHA
APPROVAL_BODY=
APPROVAL_LINK=
curl_get "selected rotation approvals" "$API_BASE/actions/runs/$SELECTED_ID/approvals" \
  APPROVAL_BODY APPROVAL_LINK
validate_approvals "$APPROVAL_BODY" "$APPROVAL_LINK" "$STEP_COMPLETED_EPOCH" "$RUN_DETAIL_CREATED"
[[ "$RECEIPT_EVIDENCE" == "$PR_RECEIPT_DIGEST" ]] ||
  die "publisher prerequisite rotation receipt digest differs"

validate_publisher_main
if ! {
  commit_exists "$RECEIPT_OLD_GATE" && commit_exists "$RECEIPT_NEW_GATE" &&
    commit_exists "$RECEIPT_HEAD"
}; then
  die "rotation receipt commits are unavailable locally"
fi
repo_git merge-base --is-ancestor "$RUN_DETAIL_HEAD" "$RECEIPT_HEAD" 2>/dev/null ||
  die "rotation dispatch is not an ancestor of final head"
repo_git merge-base --is-ancestor "$RECEIPT_NEW_GATE" "$RECEIPT_HEAD" 2>/dev/null ||
  die "new gate is not an ancestor of final head"
repo_git merge-base --is-ancestor "$RECEIPT_HEAD" "$MAIN_SHA" 2>/dev/null ||
  die "rotation final head is not an ancestor of protected main"
compare_final_gate_tree "$RECEIPT_OLD_GATE" "$RECEIPT_NEW_GATE" \
  "$RECEIPT_FINAL_GATE" "$RECEIPT_HEAD"

HISTORY_TWO=
new_temporary_file HISTORY_TWO history-two
enumerate_history "$HISTORY_TWO"
[[ "$ENUM_COUNT" == "$FIRST_COUNT" && "$ENUM_DIGEST" == "$FIRST_DIGEST" &&
  "$ENUM_SELECTED_ID" == "$FIRST_ID" && "$ENUM_SELECTED_ATTEMPT" == "$FIRST_ATTEMPT" &&
  "$ENUM_SELECTED_NUMBER" == "$FIRST_NUMBER" && "$ENUM_SELECTED_CREATED" == "$FIRST_CREATED" ]] ||
  die "rotation history selection changed between snapshots"
cmp -s "$HISTORY_ONE" "$HISTORY_TWO" || die "rotation history changed between snapshots"

RUN_BODY_TWO=
RUN_LINK_TWO=
curl_get "selected rotation run repeat" "$API_BASE/actions/runs/$SELECTED_ID" \
  RUN_BODY_TWO RUN_LINK_TWO
[[ -z "$RUN_LINK_TWO" ]] || die "repeated selected run has an unexpected Link header"
validate_run_detail "$RUN_BODY_TWO" publisher "$SELECTED_ID" "$SELECTED_ATTEMPT" \
  "$SELECTED_NUMBER" "$FIRST_CREATED" SECOND_RUN_IDENTITY
[[ "$SECOND_RUN_IDENTITY" == "$FIRST_RUN_IDENTITY" ]] ||
  die "selected rotation run detail changed during verification"
