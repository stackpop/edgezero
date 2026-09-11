#!/usr/bin/env bash
# shellcheck disable=SC2016
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SOURCE_UPDATER="$DIR/../../../docker/build-app-cli/update-image-pin-pr.sh"

pass=0
fail=0
ok() { printf '  \033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
no() { printf '  \033[31mFAIL\033[0m %s\n' "$1" >&2; fail=$((fail + 1)); }

echo '== image pin pull-request updater =='
if [[ ! -f "$SOURCE_UPDATER" ]]; then
  no 'the gate-owned updater helper exists'
  printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
  exit 1
fi

WORK=$(mktemp -d)
WORK=$(cd -- "$WORK" && pwd -P)
trap 'rm -rf -- "$WORK"' EXIT HUP INT TERM
FIXTURE_REPO="$WORK/fixture"
REMOTE="$WORK/remote.git"
GATE_ROOT="$WORK/gate"
REPOSITORY_ROOT="$WORK/source"
CASE_ROOT="$WORK/case"
FAKE_BIN="$WORK/bin"
API_ROOT="$CASE_ROOT/api"
LOG_ROOT="$CASE_ROOT/log"
TOKEN='updater-secret-token-value'
BOT_ID=4242
BOT_LOGIN='edgezero-publisher[bot]'
SOURCE_PR=77
COMMENT_ID=88
TAG=build-container-v7
DIGEST="sha256:$(printf '1%.0s' {1..64})"
OLD_DIGEST="sha256:$(printf '2%.0s' {1..64})"
CHALLENGE=$(printf '3%.0s' {1..64})
SCREENSHOT="sha256:$(printf '4%.0s' {1..64})"
REVIEWED_AT=2026-09-10T12:00:00Z
REAL_GIT=$(command -v git)
REAL_JQ=$(command -v jq)
REAL_BASH=$(command -v bash)
REAL_ENV=$(command -v env)
REAL_MKTEMP=$(command -v mktemp)
export FIXTURE_REPO REMOTE CASE_ROOT API_ROOT LOG_ROOT REAL_GIT REAL_JQ BOT_ID BOT_LOGIN TOKEN
printf '%s' "$REAL_GIT" >"$WORK/real-git"
printf '%s' "$REAL_JQ" >"$WORK/real-jq"
printf '%s' "$REAL_MKTEMP" >"$WORK/real-mktemp"
printf '%s' "$TOKEN" >"$WORK/token"
printf '%s' "$BOT_ID" >"$WORK/bot-id"
printf '%s' "$BOT_LOGIN" >"$WORK/bot-login"

git_fixture() {
  env PATH="$PATH" HOME="$WORK/home" LC_ALL=C \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
    "$REAL_GIT" -C "$FIXTURE_REPO" "$@"
}

write_pair() {
  local root=$1 source=$2 digest=$3
  mkdir -p "$root/.github/docker/build-app-cli"
  printf '%s' "{\"digest\":\"$digest\",\"image-source-revision\":\"$source\",\"provenance-protocol\":1,\"repository\":\"ghcr.io/stackpop/edgezero-build-app-cli\",\"tag\":\"$TAG\"}" \
    >"$root/.github/docker/build-app-cli/image.json"
  printf '%s' "{\"approval-challenge\":\"$CHALLENGE\",\"approver-login\":\"release-reviewer\",\"image-digest\":\"$digest\",\"release-tag\":\"$TAG\",\"reviewed-at\":\"$REVIEWED_AT\",\"run-attempt\":\"2\",\"run-id\":\"9007199254740993\",\"schema-version\":1,\"screenshot-sha256\":\"$SCREENSHOT\",\"source-revision\":\"$source\"}" \
    >"$root/.github/docker/build-app-cli/image-release-evidence.json"
}

mkdir -p "$WORK/home" "$FIXTURE_REPO/.github/docker/build-app-cli" "$FAKE_BIN"
"$REAL_GIT" init -q "$FIXTURE_REPO"
cp "$SOURCE_UPDATER" "$FIXTURE_REPO/.github/docker/build-app-cli/update-image-pin-pr.sh"
cp "$DIR/../../../docker/build-app-cli/check-image-pin.sh" \
  "$FIXTURE_REPO/.github/docker/build-app-cli/check-image-pin.sh"
cp "$DIR/../../../docker/build-app-cli/write-image-release-record.sh" \
  "$FIXTURE_REPO/.github/docker/build-app-cli/write-image-release-record.sh"
chmod 0755 "$FIXTURE_REPO/.github/docker/build-app-cli/"*.sh
git_fixture add .github/docker/build-app-cli
git_fixture -c user.name=fixture -c user.email=fixture@example.invalid commit -q -m gate
G=$(git_fixture rev-parse HEAD)
git_fixture -c user.name=fixture -c user.email=fixture@example.invalid commit -q --allow-empty -m previous
P=$(git_fixture rev-parse HEAD)
write_pair "$FIXTURE_REPO" "$P" "$OLD_DIGEST"
git_fixture add .github/docker/build-app-cli/image.json \
  .github/docker/build-app-cli/image-release-evidence.json
git_fixture -c user.name=fixture -c user.email=fixture@example.invalid commit -q -m base-pin
I=$(git_fixture rev-parse HEAD)
git_fixture -c user.name=fixture -c user.email=fixture@example.invalid commit -q --allow-empty -m source
S=$(git_fixture rev-parse HEAD)
git_fixture -c user.name=fixture -c user.email=fixture@example.invalid commit -q --allow-empty -m newer
N=$(git_fixture rev-parse HEAD)
git_fixture checkout -q --detach "$S"
write_pair "$FIXTURE_REPO" "$S" "$DIGEST"
git_fixture add .github/docker/build-app-cli/image.json \
  .github/docker/build-app-cli/image-release-evidence.json
git_fixture -c user.name=fixture -c user.email=fixture@example.invalid commit -q -m equal-main
B_EQUAL=$(git_fixture rev-parse HEAD)
git_fixture checkout -q --detach "$N"
write_pair "$FIXTURE_REPO" "$N" "$DIGEST"
git_fixture add .github/docker/build-app-cli/image.json \
  .github/docker/build-app-cli/image-release-evidence.json
git_fixture -c user.name=fixture -c user.email=fixture@example.invalid commit -q -m newer-main
B_NEWER=$(git_fixture rev-parse HEAD)
git_fixture checkout -q --detach "$G"
git_fixture -c user.name=fixture -c user.email=fixture@example.invalid commit -q --allow-empty -m side
Q=$(git_fixture rev-parse HEAD)
write_pair "$FIXTURE_REPO" "$Q" "$DIGEST"
git_fixture add .github/docker/build-app-cli/image.json \
  .github/docker/build-app-cli/image-release-evidence.json
git_fixture -c user.name=fixture -c user.email=fixture@example.invalid commit -q -m side-main
B_SIDE=$(git_fixture rev-parse HEAD)

"$REAL_GIT" clone -q --bare "$FIXTURE_REPO" "$REMOTE" >/dev/null 2>&1
"$REAL_GIT" clone -q "$FIXTURE_REPO" "$GATE_ROOT" >/dev/null 2>&1
"$REAL_GIT" -C "$GATE_ROOT" checkout -q --detach "$G" >/dev/null 2>&1
"$REAL_GIT" clone -q "$FIXTURE_REPO" "$REPOSITORY_ROOT" >/dev/null 2>&1
"$REAL_GIT" -C "$REPOSITORY_ROOT" checkout -q --detach "$S" >/dev/null 2>&1

cat >"$FAKE_BIN/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
ROOT=${0%/bin/git}
REMOTE="$ROOT/remote.git"
CASE_ROOT="$ROOT/case"
LOG_ROOT="$CASE_ROOT/log"
REAL_GIT=$(<"$ROOT/real-git")
audit_lazy_fetch=true
for transport_argument in "$@"; do
  case "$transport_argument" in
    upload-pack | receive-pack | */records) audit_lazy_fetch=false ;;
  esac
done
for variable in EDGEZERO_BUILD_CONTAINER_APP_TOKEN GITHUB_TOKEN GH_TOKEN TOKEN APP_TOKEN APP_TOKEN_LOCAL; do
  eval "present=\${$variable+x}"
  [[ -z "$present" ]] || printf '%s\n' "$variable" >>"$LOG_ROOT/environment-leaks"
done
[[ "$-" != *x* && "$-" != *a* ]] || printf '%s\n' shell-options >>"$LOG_ROOT/environment-leaks"
if [[ "$audit_lazy_fetch" == true ]]; then
  [[ "${GIT_NO_LAZY_FETCH:-}" == 1 ]] || printf '%s\n' git-lazy-fetch >>"$LOG_ROOT/environment-leaks"
fi
printf '%s\0' "$@" >>"$LOG_ROOT/git.argv"
printf '\n' >>"$LOG_ROOT/git.argv"
args=("$@")
operation=
for argument in "$@"; do
  case "$argument" in ls-remote|fetch|push) operation=$argument; break ;; esac
done
if [[ "$operation" == ls-remote ]]; then
  for index in "${!args[@]}"; do
    [[ "${args[index]}" == https://github.com/stackpop/edgezero.git ]] && args[index]=$REMOTE
  done
elif [[ "$operation" == fetch || "$operation" == push ]]; then
  for index in "${!args[@]}"; do
    [[ "${args[index]}" == origin ]] && args[index]=$REMOTE
  done
fi
if [[ -n "$operation" ]]; then
  mode=
  if mode=$(stat -f '%OLp' -- "${GIT_ASKPASS:-}" 2>/dev/null); then :; else mode=$(stat -c '%a' -- "${GIT_ASKPASS:-}" 2>/dev/null || true); fi
  printf '%s\t%s\n' "${GIT_TERMINAL_PROMPT:-}" "$mode" >>"$LOG_ROOT/git-auth"
fi
if [[ "$operation" == push && -f "$CASE_ROOT/race-create" ]]; then
  branch=$(<"$CASE_ROOT/race-create")
  "$REAL_GIT" --git-dir="$REMOTE" update-ref "refs/heads/$branch" "$S"
  rm -f -- "$CASE_ROOT/race-create"
fi
if [[ "$operation" == push && -f "$CASE_ROOT/race-update-existing" ]]; then
  branch=$(sed -n '1p' "$CASE_ROOT/race-update-existing")
  winner=$(sed -n '2p' "$CASE_ROOT/race-update-existing")
  "$REAL_GIT" --git-dir="$REMOTE" update-ref "refs/heads/$branch" "$winner"
  rm -f -- "$CASE_ROOT/race-update-existing"
fi
if [[ "$operation" == push && -f "$CASE_ROOT/race-readback" ]]; then
  "$REAL_GIT" "${args[@]}"
  status=$?
  branch=$(<"$CASE_ROOT/race-readback")
  "$REAL_GIT" --git-dir="$REMOTE" update-ref "refs/heads/$branch" "$S"
  rm -f -- "$CASE_ROOT/race-readback"
  exit "$status"
fi
if [[ "$operation" == ls-remote && -f "$CASE_ROOT/race-final-main" ]]; then
  reads_main=false
  reads_target=false
  for argument in "${args[@]}"; do
    [[ "$argument" != refs/heads/main ]] || reads_main=true
    [[ "$argument" != refs/heads/edgezero-build-container-pin/* ]] || reads_target=true
  done
  if [[ "$reads_main" == true && "$reads_target" == true ]]; then
    combined_count=0
    [[ ! -f "$CASE_ROOT/combined-ref-count" ]] || combined_count=$(<"$CASE_ROOT/combined-ref-count")
    combined_count=$((combined_count + 1))
    printf '%s' "$combined_count" >"$CASE_ROOT/combined-ref-count"
    if ((combined_count >= 2)); then
      "$REAL_GIT" --git-dir="$REMOTE" update-ref refs/heads/main "$(<"$CASE_ROOT/race-final-main")"
      rm -f -- "$CASE_ROOT/race-final-main"
    fi
  fi
fi
exec "$REAL_GIT" "${args[@]}"
EOF
chmod 0755 "$FAKE_BIN/git"

cat >"$FAKE_BIN/mktemp" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
ROOT=${0%/bin/mktemp}
if [[ -f "$ROOT/case/fail-mktemp" ]]; then
  exit 1
fi
exec "$(<"$ROOT/real-mktemp")" "$@"
EOF
chmod 0755 "$FAKE_BIN/mktemp"

cat >"$FAKE_BIN/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
ROOT=${0%/bin/curl}
REMOTE="$ROOT/remote.git"
CASE_ROOT="$ROOT/case"
API_ROOT="$CASE_ROOT/api"
LOG_ROOT="$CASE_ROOT/log"
REAL_GIT=$(<"$ROOT/real-git")
REAL_JQ=$(<"$ROOT/real-jq")
BOT_ID=$(<"$ROOT/bot-id")
BOT_LOGIN=$(<"$ROOT/bot-login")
for variable in EDGEZERO_BUILD_CONTAINER_APP_TOKEN GITHUB_TOKEN GH_TOKEN TOKEN APP_TOKEN APP_TOKEN_LOCAL; do
  eval "present=\${$variable+x}"
  [[ -z "$present" ]] || printf '%s\n' "$variable" >>"$LOG_ROOT/environment-leaks"
done
[[ "$-" != *x* && "$-" != *a* ]] || printf '%s\n' shell-options >>"$LOG_ROOT/environment-leaks"
count=0
[[ ! -f "$LOG_ROOT/curl-count" ]] || count=$(<"$LOG_ROOT/curl-count")
count=$((count + 1))
printf '%s' "$count" >"$LOG_ROOT/curl-count"
call="$LOG_ROOT/curl-$count"
mkdir -p "$call"
printf '%s\n' "$@" >"$call/args"
cat >"$call/config"
method= output= data= url=
while (($#)); do
  case "$1" in
    --request) method=$2; shift 2 ;;
    --output) output=$2; shift 2 ;;
    --data-binary)
      data=$2
      shift 2
      ;;
    --connect-timeout|--max-time|--max-redirs|--config|--write-out) shift 2 ;;
    --disable|--silent|--show-error) shift ;;
    https://*) url=$1; shift ;;
    *) shift ;;
  esac
done
printf '%s' "$method" >"$call/method"
printf '%s' "$url" >"$call/url"
if [[ -n "$data" ]]; then
  [[ "$data" == @* ]] || exit 91
  cp "${data#@}" "$call/body"
else
  : >"$call/body"
fi
path=${url#https://api.github.com}
if [[ -f "$CASE_ROOT/fail-next" ]]; then
  failure=$(<"$CASE_ROOT/fail-next")
  if [[ "$method $path" == *"$failure"* ]]; then
    rm -f -- "$CASE_ROOT/fail-next"
    printf '{}'>"$output"
    printf '500\n2026-03-10\napplication/json\n\n'
    exit 0
  fi
fi
if [[ -f "$CASE_ROOT/signal-next" ]]; then
  temp_root=${output%/api/*}
  printf '%s' "$temp_root" >"$CASE_ROOT/signal-temp-root"
  signal=TERM
  [[ ! -s "$CASE_ROOT/signal-next" ]] || signal=$(<"$CASE_ROOT/signal-next")
  kill "-$signal" "$PPID"
  sleep 2
  exit 94
fi
status=200
link=
case "$method $path" in
  "GET /users/${BOT_LOGIN%\[bot\]}%5Bbot%5D") cp "$API_ROOT/user.json" "$output" ;;
  'GET /repos/stackpop/edgezero') cp "$API_ROOT/repo.json" "$output" ;;
  GET\ /repos/stackpop/edgezero/pulls\?*)
    page=${path##*page=}
    [[ "$page" =~ ^([1-9]|[1-9][0-9]|100)$ ]] || exit 92
    expected="/repos/stackpop/edgezero/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=$page"
    [[ "$path" == "$expected" ]] || exit 92
    if [[ -f "$API_ROOT/full-pages" ]]; then
      jq -cn --argjson page "$page" '[range(0;100) | {id:(($page-1)*100+.+1),number:(($page-1)*100+.+1),title:"ordinary",head:{ref:"ordinary"}}]' >"$output"
    elif [[ -f "$API_ROOT/pulls-page-$page.json" ]]; then
      cp "$API_ROOT/pulls-page-$page.json" "$output"
    else
      printf '[]' >"$output"
    fi
    [[ ! -f "$API_ROOT/link-page-$page" ]] || link=$(<"$API_ROOT/link-page-$page")
    ;;
  GET\ /repos/stackpop/edgezero/pulls/*)
    number=${path##*/}
    [[ "$number" =~ ^[1-9][0-9]*$ ]] || exit 93
    pull_get_count=0
    [[ ! -f "$API_ROOT/pull-$number-get-count" ]] || \
      pull_get_count=$(<"$API_ROOT/pull-$number-get-count")
    pull_get_count=$((pull_get_count + 1))
    printf '%s' "$pull_get_count" >"$API_ROOT/pull-$number-get-count"
    if [[ -f "$CASE_ROOT/race-final-pull" && $(<"$CASE_ROOT/race-final-pull") == "$number" &&
      "$pull_get_count" -ge 2 ]]; then
      jq '.user.id='"$((BOT_ID + 1))" "$API_ROOT/pull-$number.json" \
        >"$API_ROOT/pull-$number.next"
      mv "$API_ROOT/pull-$number.next" "$API_ROOT/pull-$number.json"
      rm -f -- "$CASE_ROOT/race-final-pull"
    fi
    head_ref=$(jq -er '.head.ref' "$API_ROOT/pull-$number.json")
    if head_sha=$("$REAL_GIT" --git-dir="$REMOTE" rev-parse "refs/heads/$head_ref" 2>/dev/null); then
      jq --arg head_sha "$head_sha" '.head.sha=$head_sha' \
        "$API_ROOT/pull-$number.json" >"$API_ROOT/pull-$number.next"
      mv "$API_ROOT/pull-$number.next" "$API_ROOT/pull-$number.json"
    fi
    cp "$API_ROOT/pull-$number.json" "$output"
    ;;
  POST\ /repos/stackpop/edgezero/pulls)
    status=201
    number=900
    body=$(<"$call/body")
    ref=$(jq -er '.head' "$call/body")
    sha=$("$REAL_GIT" --git-dir="$REMOTE" rev-parse "refs/heads/$ref")
    jq -cn --argjson number "$number" --argjson id 9900 --argjson bot "$BOT_ID" \
      --arg login "$BOT_LOGIN" --arg sha "$sha" --argjson request "$body" \
      '{id:$id,number:$number,state:"open",merged:false,merged_at:null,merge_commit_sha:null,
        user:{id:$bot,login:$login,type:"Bot"},title:$request.title,body:$request.body,
        base:{ref:$request.base,repo:{full_name:"stackpop/edgezero"}},
        head:{ref:$request.head,sha:$sha,repo:{full_name:"stackpop/edgezero"}}}' \
      >"$API_ROOT/pull-$number.json"
    if [[ -f "$CASE_ROOT/created-merge-sha" ]]; then
      "$REAL_JQ" --arg merge_sha "$(<"$CASE_ROOT/created-merge-sha")" \
        '.merge_commit_sha=$merge_sha' "$API_ROOT/pull-$number.json" \
        >"$API_ROOT/pull-$number.next"
      mv "$API_ROOT/pull-$number.next" "$API_ROOT/pull-$number.json"
    fi
    "$REAL_JQ" --slurpfile pull "$API_ROOT/pull-$number.json" \
      '. + [($pull[0] | {id,number,title,head:{ref:.head.ref}})]' \
      "$API_ROOT/pulls-page-1.json" >"$API_ROOT/pulls-page-1.next"
    mv "$API_ROOT/pulls-page-1.next" "$API_ROOT/pulls-page-1.json"
    if [[ -f "$CASE_ROOT/race-post-head" ]]; then
      "$REAL_GIT" --git-dir="$REMOTE" update-ref "refs/heads/$ref" "$(<"$CASE_ROOT/race-post-head")"
      rm -f -- "$CASE_ROOT/race-post-head"
    fi
    cp "$API_ROOT/pull-$number.json" "$output"
    ;;
  PATCH\ /repos/stackpop/edgezero/pulls/*)
    number=${path##*/}
    [[ "$number" =~ ^[1-9][0-9]*$ ]] || exit 93
    request=$(<"$call/body")
    head_ref=$(jq -er '.head.ref' "$API_ROOT/pull-$number.json")
    head_sha=$("$REAL_GIT" --git-dir="$REMOTE" rev-parse "refs/heads/$head_ref")
    jq --argjson request "$request" --arg head_sha "$head_sha" \
      '.head.sha=$head_sha | .state=($request.state // .state) | .title=($request.title // .title) |
       .body=($request.body // .body) | .base.ref=($request.base // .base.ref)' \
      "$API_ROOT/pull-$number.json" >"$API_ROOT/pull-$number.next"
    mv "$API_ROOT/pull-$number.next" "$API_ROOT/pull-$number.json"
    cp "$API_ROOT/pull-$number.json" "$output"
    ;;
  *) exit 93 ;;
esac
content_type=application/json
version=2026-03-10
[[ ! -f "$API_ROOT/content-type" ]] || content_type=$(<"$API_ROOT/content-type")
[[ ! -f "$API_ROOT/version" ]] || version=$(<"$API_ROOT/version")
if [[ -f "$CASE_ROOT/response-status" ]]; then
  read -r override_method override_status <"$CASE_ROOT/response-status"
  if [[ "$method" == "$override_method" ]]; then
    status=$override_status
    rm -f -- "$CASE_ROOT/response-status"
  fi
fi
printf '%s\n%s\n%s\n%s' "$status" "$version" "$content_type" "$link"
EOF
chmod 0755 "$FAKE_BIN/curl"

cat >"$FAKE_BIN/jq" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
ROOT=${0%/bin/jq}
LOG_ROOT="$ROOT/case/log"
REAL_JQ=$(<"$ROOT/real-jq")
for variable in EDGEZERO_BUILD_CONTAINER_APP_TOKEN GITHUB_TOKEN GH_TOKEN TOKEN APP_TOKEN APP_TOKEN_LOCAL; do
  eval "present=\${$variable+x}"
  [[ -z "$present" ]] || printf '%s\n' "$variable" >>"$LOG_ROOT/environment-leaks"
done
[[ "$-" != *x* && "$-" != *a* ]] || printf '%s\n' shell-options >>"$LOG_ROOT/environment-leaks"
exec "$REAL_JQ" "$@"
EOF
chmod 0755 "$FAKE_BIN/jq"

remote_ref() {
  "$REAL_GIT" --git-dir="$REMOTE" rev-parse --verify "$1" 2>/dev/null || true
}

reset_remote() {
  local main=$1 ref
  "$REAL_GIT" --git-dir="$REMOTE" update-ref refs/heads/main "$main"
  while IFS= read -r ref; do
    [[ -z "$ref" ]] || "$REAL_GIT" --git-dir="$REMOTE" update-ref -d "$ref"
  done < <("$REAL_GIT" --git-dir="$REMOTE" for-each-ref --format='%(refname)' \
    refs/heads/edgezero-build-container-pin/)
}

set_pin_branch_mode() {
  local source=$1 path=$2 work="$WORK/mode-work"
  rm -rf -- "$work"
  "$REAL_GIT" clone -q "$REMOTE" "$work" >/dev/null 2>&1
  "$REAL_GIT" -C "$work" checkout -q "edgezero-build-container-pin/$source"
  "$REAL_GIT" -C "$work" update-index --chmod=+x -- "$path"
  "$REAL_GIT" -C "$work" -c user.name="$BOT_LOGIN" \
    -c user.email="$BOT_ID+$BOT_LOGIN@users.noreply.github.com" \
    -c commit.gpgsign=false commit -q --amend --no-edit --no-gpg-sign
  TARGET_OID=$("$REAL_GIT" -C "$work" rev-parse HEAD)
  "$REAL_GIT" -C "$work" push -q --force "$REMOTE" \
    "HEAD:refs/heads/edgezero-build-container-pin/$source"
}

write_approval() {
  local source=$1 digest=$2
  APPROVAL="$CASE_ROOT/approval.json"
  printf '%s' "{\"approval-challenge\":\"$CHALLENGE\",\"approver-login\":\"release-reviewer\",\"image-digest\":\"$digest\",\"release-tag\":\"$TAG\",\"reviewed-at\":\"$REVIEWED_AT\",\"run-attempt\":\"2\",\"run-id\":\"9007199254740993\",\"schema-version\":1,\"screenshot-sha256\":\"$SCREENSHOT\",\"source-revision\":\"$source\"}" >"$APPROVAL"
}

new_case() {
  local main=${1:-$G} source=${2:-$S} digest=${3:-$DIGEST}
  rm -rf -- "$CASE_ROOT"
  mkdir -m 0700 "$CASE_ROOT"
  mkdir -p "$API_ROOT" "$LOG_ROOT"
  reset_remote "$main"
  printf '%s' "{\"id\":$BOT_ID,\"login\":\"$BOT_LOGIN\",\"type\":\"Bot\"}" >"$API_ROOT/user.json"
  printf '%s' '{"id":101,"full_name":"stackpop/edgezero","private":false,"visibility":"public","default_branch":"main","owner":{"login":"stackpop"}}' >"$API_ROOT/repo.json"
  printf '[]' >"$API_ROOT/pulls-page-1.json"
  RUN_SOURCE=$source
  RUN_DIGEST=$digest
  write_approval "$source" "$digest"
}

expected_title() { printf 'chore(actions): pin build container for %s' "$1"; }
expected_body() {
  local source=$1 digest=$2
  printf 'edgezero-build-container-pin-v1 {"evidence-url":"https://github.com/stackpop/edgezero/pull/%s#issuecomment-%s","image-digest":"%s","release-tag":"%s","source-pr":"%s","source-revision":"%s"}' \
    "$SOURCE_PR" "$COMMENT_ID" "$digest" "$TAG" "$SOURCE_PR" "$source"
}

create_pin_branch() {
  local source=$1 digest=$2 parent=${3:-$G} branch work
  branch="edgezero-build-container-pin/$source"
  work="$WORK/branch-work"
  rm -rf -- "$work"
  "$REAL_GIT" clone -q "$REMOTE" "$work" >/dev/null 2>&1
  "$REAL_GIT" -C "$work" checkout -q --detach "$parent"
  write_pair "$work" "$source" "$digest"
  "$REAL_GIT" -C "$work" add .github/docker/build-app-cli/image.json \
    .github/docker/build-app-cli/image-release-evidence.json
  if "$REAL_GIT" -C "$work" diff --cached --quiet; then
    TARGET_OID=$parent
    "$REAL_GIT" --git-dir="$REMOTE" update-ref "refs/heads/$branch" "$TARGET_OID"
    return
  fi
  "$REAL_GIT" -C "$work" -c user.name="$BOT_LOGIN" \
    -c user.email="$BOT_ID+$BOT_LOGIN@users.noreply.github.com" \
    -c commit.gpgsign=false commit -q --no-gpg-sign -m "$(expected_title "$source")"
  TARGET_OID=$("$REAL_GIT" -C "$work" rev-parse HEAD)
  "$REAL_GIT" -C "$work" push -q "$REMOTE" "HEAD:refs/heads/$branch"
}

create_mismatched_pin_branch() {
  local branch_source=$1 record_source=$2 digest=$3 parent=${4:-$G} branch work
  branch="edgezero-build-container-pin/$branch_source"
  work="$WORK/branch-work"
  rm -rf -- "$work"
  "$REAL_GIT" clone -q "$REMOTE" "$work" >/dev/null 2>&1
  "$REAL_GIT" -C "$work" checkout -q --detach "$parent"
  write_pair "$work" "$record_source" "$digest"
  "$REAL_GIT" -C "$work" add .github/docker/build-app-cli/image.json \
    .github/docker/build-app-cli/image-release-evidence.json
  "$REAL_GIT" -C "$work" -c user.name="$BOT_LOGIN" \
    -c user.email="$BOT_ID+$BOT_LOGIN@users.noreply.github.com" \
    -c commit.gpgsign=false commit -q --no-gpg-sign -m "$(expected_title "$branch_source")"
  TARGET_OID=$("$REAL_GIT" -C "$work" rev-parse HEAD)
  "$REAL_GIT" -C "$work" push -q "$REMOTE" "HEAD:refs/heads/$branch"
}

set_pin_pr() {
  local number=$1 source=$2 digest=$3 state=$4 merged=$5
  local author_id=${6:-$BOT_ID} author_login=${7:-$BOT_LOGIN} head_repo=${8:-stackpop/edgezero}
  local branch sha body title merged_at merge_sha
  branch="edgezero-build-container-pin/$source"
  sha=$(remote_ref "refs/heads/$branch")
  [[ -n "$sha" ]] || sha=$source
  body=$(expected_body "$source" "$digest")
  title=$(expected_title "$source")
  if [[ "$merged" == true ]]; then merged_at='"2026-09-10T13:00:00Z"'; merge_sha="\"$(remote_ref refs/heads/main)\""; else merged_at=null; merge_sha=null; fi
  jq -cn --argjson number "$number" --argjson id "$((number + 1000))" \
    --argjson author_id "$author_id" --arg author_login "$author_login" \
    --arg head_repo "$head_repo" --arg branch "$branch" --arg sha "$sha" \
    --arg title "$title" --arg body "$body" --arg state "$state" \
    --argjson merged "$merged" --argjson merged_at "$merged_at" --argjson merge_sha "$merge_sha" \
    '{id:$id,number:$number,state:$state,merged:$merged,merged_at:$merged_at,
      merge_commit_sha:$merge_sha,user:{id:$author_id,login:$author_login,type:"Bot"},
      title:$title,body:$body,base:{ref:"main",repo:{full_name:"stackpop/edgezero"}},
      head:{ref:$branch,sha:$sha,repo:{full_name:$head_repo}}}' >"$API_ROOT/pull-$number.json"
  jq -cn --argjson number "$number" --argjson id "$((number + 1000))" \
    --arg title "$title" --arg branch "$branch" '[{id:$id,number:$number,title:$title,head:{ref:$branch}}]' \
    >"$API_ROOT/pulls-page-1.json"
}

append_pin_pr() {
  local number=$1 source=$2 digest=$3 state=$4 merged=$5 item
  set_pin_pr "$number" "$source" "$digest" "$state" "$merged"
  item=$(<"$API_ROOT/pulls-page-1.json")
  if [[ -f "$CASE_ROOT/saved-list" ]]; then
    jq -cn --argjson old "$(<"$CASE_ROOT/saved-list")" --argjson new "$item" '$old + $new' >"$API_ROOT/pulls-page-1.json"
  fi
  cp "$API_ROOT/pulls-page-1.json" "$CASE_ROOT/saved-list"
}

run_updater() {
  local supplied_token=${1:-$TOKEN}
  env -i PATH="$FAKE_BIN:$PATH" LC_ALL=C EDGEZERO_BUILD_CONTAINER_APP_TOKEN="$supplied_token" \
    bash "$GATE_ROOT/.github/docker/build-app-cli/update-image-pin-pr.sh" \
      --gate-root "$GATE_ROOT" \
      --gate-sha "$G" \
      --repository-root "$REPOSITORY_ROOT" \
      --source-revision "$RUN_SOURCE" \
      --release-tag "$TAG" \
      --image-digest "$RUN_DIGEST" \
      --provenance-protocol 1 \
      --approval-json "$APPROVAL" \
      --source-pr "$SOURCE_PR" \
      --evidence-url "https://github.com/stackpop/edgezero/pull/$SOURCE_PR#issuecomment-$COMMENT_ID" \
      --expected-bot-id "$BOT_ID" \
      --expected-bot-login "$BOT_LOGIN"
}

run_updater_hostile_environment() {
  env -i PATH="$FAKE_BIN:$PATH" LC_ALL=C SHELLOPTS=xtrace:allexport \
    EDGEZERO_BUILD_CONTAINER_APP_TOKEN="$TOKEN" \
    GITHUB_TOKEN=ambient-github-token GH_TOKEN=ambient-gh-token TOKEN=ambient-token-alias \
    APP_TOKEN=ambient-app-token-alias APP_TOKEN_LOCAL=ambient-local-token-alias \
    bash "$GATE_ROOT/.github/docker/build-app-cli/update-image-pin-pr.sh" \
      --gate-root "$GATE_ROOT" \
      --gate-sha "$G" \
      --repository-root "$REPOSITORY_ROOT" \
      --source-revision "$RUN_SOURCE" \
      --release-tag "$TAG" \
      --image-digest "$RUN_DIGEST" \
      --provenance-protocol 1 \
      --approval-json "$APPROVAL" \
      --source-pr "$SOURCE_PR" \
      --evidence-url "https://github.com/stackpop/edgezero/pull/$SOURCE_PR#issuecomment-$COMMENT_ID" \
      --expected-bot-id "$BOT_ID" \
      --expected-bot-login "$BOT_LOGIN"
}

run_updater_with_environment_alternate() {
  env -i PATH="$FAKE_BIN:$PATH" LC_ALL=C \
    GIT_ALTERNATE_OBJECT_DIRECTORIES="$GATE_ROOT/.git/objects" \
    EDGEZERO_BUILD_CONTAINER_APP_TOKEN="$TOKEN" \
    bash "$GATE_ROOT/.github/docker/build-app-cli/update-image-pin-pr.sh" \
      --gate-root "$GATE_ROOT" \
      --gate-sha "$G" \
      --repository-root "$REPOSITORY_ROOT" \
      --source-revision "$RUN_SOURCE" \
      --release-tag "$TAG" \
      --image-digest "$RUN_DIGEST" \
      --provenance-protocol 1 \
      --approval-json "$APPROVAL" \
      --source-pr "$SOURCE_PR" \
      --evidence-url "https://github.com/stackpop/edgezero/pull/$SOURCE_PR#issuecomment-$COMMENT_ID" \
      --expected-bot-id "$BOT_ID" \
      --expected-bot-login "$BOT_LOGIN"
}

run_updater_without_tool() {
  local missing=$1 closed_path="$WORK/path-without-$1" tool resolved
  rm -rf -- "$closed_path"
  mkdir -p "$closed_path"
  for tool in bash env git jq curl mktemp stat chmod rm cmp install wc tr sed awk sort uniq mkdir ln \
    cat cp dirname basename; do
    [[ "$tool" != "$missing" ]] || continue
    case "$tool" in
      git | jq | curl) resolved="$FAKE_BIN/$tool" ;;
      *) resolved=$(command -v "$tool") ;;
    esac
    ln -s "$resolved" "$closed_path/$tool"
  done
  "$REAL_ENV" -i PATH="$closed_path" LC_ALL=C EDGEZERO_BUILD_CONTAINER_APP_TOKEN="$TOKEN" \
    "$REAL_BASH" "$GATE_ROOT/.github/docker/build-app-cli/update-image-pin-pr.sh" \
      --gate-root "$GATE_ROOT" \
      --gate-sha "$G" \
      --repository-root "$REPOSITORY_ROOT" \
      --source-revision "$RUN_SOURCE" \
      --release-tag "$TAG" \
      --image-digest "$RUN_DIGEST" \
      --provenance-protocol 1 \
      --approval-json "$APPROVAL" \
      --source-pr "$SOURCE_PR" \
      --evidence-url "https://github.com/stackpop/edgezero/pull/$SOURCE_PR#issuecomment-$COMMENT_ID" \
      --expected-bot-id "$BOT_ID" \
      --expected-bot-login "$BOT_LOGIN"
}

capture() {
  local status=0
  "$@" >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr" || status=$?
  printf '%s' "$status"
}

assert_status() {
  local expected=$1 description=$2
  shift 2
  local status
  status=$(capture "$@")
  if [[ "$status" == "$expected" ]]; then ok "$description"; else
    cat "$CASE_ROOT/stdout" "$CASE_ROOT/stderr" >&2
    no "$description (status $status, expected $expected)"
  fi
}

assert_silent_sanitized() {
  local description=$1
  if [[ ! -s "$CASE_ROOT/stdout" && $(<"$CASE_ROOT/stderr") != *"$TOKEN"* ]]; then
    ok "$description"
  else
    cat "$CASE_ROOT/stdout" "$CASE_ROOT/stderr" >&2
    no "$description"
  fi
}

mutation_count() {
  local count=0 call method
  for call in "$LOG_ROOT"/curl-*; do
    [[ -d "$call" ]] || continue
    method=$(<"$call/method")
    [[ "$method" == GET ]] || count=$((count + 1))
  done
  printf '%s' "$count"
}

assert_no_mutation() {
  local description=$1
  if [[ $(mutation_count) == 0 ]]; then ok "$description"; else no "$description"; fi
}

assert_no_network() {
  local description=$1
  if [[ ! -f "$LOG_ROOT/curl-count" ]]; then ok "$description"; else no "$description"; fi
}

assert_api_transport() {
  local good=true call method url output request actual expected expected_config
  for call in "$LOG_ROOT"/curl-*; do
    [[ -d "$call" ]] || continue
    method=$(<"$call/method")
    expected_config="$CASE_ROOT/expected-config"
    printf '%s\n' \
      'header = "Accept: application/vnd.github+json"' \
      'header = "X-GitHub-Api-Version: 2026-03-10"' \
      'header = "User-Agent: edgezero-build-container-gate/1"' \
      "header = \"Authorization: Bearer $TOKEN\"" >"$expected_config"
    [[ "$method" == GET ]] || printf '%s\n' 'header = "Content-Type: application/json"' >>"$expected_config"
    if ! cmp -s "$expected_config" "$call/config"; then
      printf 'curl config mismatch for %s %s\n' "$method" "$(<"$call/url")" >&2
      diff -u "$expected_config" "$call/config" >&2 || true
      good=false
    fi
    url=$(<"$call/url")
    if [[ "$method" == GET ]]; then
      output=$(sed -n '15p' "$call/args")
      actual=$(<"$call/args")
      expected=$(printf '%s\n' \
        --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
        --request "$method" --config - --output "$output" --write-out \
        '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}\n%header{link}' "$url")
    else
      request=$(sed -n '15p' "$call/args")
      output=$(sed -n '17p' "$call/args")
      actual=$(<"$call/args")
      expected=$(printf '%s\n' \
        --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
        --request "$method" --config - --data-binary "$request" --output "$output" --write-out \
        '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}\n%header{link}' "$url")
      [[ "$request" == @/private/tmp/edgezero-image-pin.??????/api/*.json ||
        "$request" == @/tmp/edgezero-image-pin.??????/api/*.json ]] || good=false
    fi
    [[ "$output" == /private/tmp/edgezero-image-pin.??????/api/body.?????? ||
      "$output" == /tmp/edgezero-image-pin.??????/api/body.?????? ]] || good=false
    if [[ "$actual" != "$expected" ]]; then
      printf 'curl argv mismatch for %s %s\nexpected:\n%s\nactual:\n%s\n' \
        "$method" "$url" "$expected" "$actual" >&2
      good=false
    fi
  done
  if [[ "$good" == true ]]; then ok 'every REST call uses the complete ordered curl argv and exact config'; else no 'every REST call uses the complete ordered curl argv and exact config'; fi
}

assert_request_order() {
  local expected=$1 description=$2 actual='' call
  for call in "$LOG_ROOT"/curl-*; do
    [[ -d "$call" ]] || continue
    actual+="$(<"$call/method") $(<"$call/url")"
    actual+=$'\n'
  done
  if [[ "${actual%$'\n'}" == "$expected" ]]; then ok "$description"; else
    printf 'expected:\n%s\nactual:\n%b' "$expected" "$actual" >&2
    no "$description"
  fi
}

new_case "$G"
assert_status 0 'an absent target branch creates the exact pin proposal' run_updater
assert_silent_sanitized 'successful creation is silent and keeps the token out of output'
CREATED_OID=$(remote_ref "refs/heads/edgezero-build-container-pin/$S")
if [[ -n "$CREATED_OID" && $("$REAL_GIT" --git-dir="$REMOTE" show -s --format=%P "$CREATED_OID") == "$G" ]]; then
  ok 'the created commit has the recorded remote main as its sole parent'
else
  no 'the created commit has the recorded remote main as its sole parent'
fi
if [[ $("$REAL_GIT" --git-dir="$REMOTE" show -s --format='%s%n%an%n%ae' "$CREATED_OID") == "$(expected_title "$S")"$'\n'"$BOT_LOGIN"$'\n'"$BOT_ID+$BOT_LOGIN@users.noreply.github.com" ]]; then
  ok 'the commit is unsigned, hook-free, and uses the fixed title and bot identity'
else
  no 'the commit is unsigned, hook-free, and uses the fixed title and bot identity'
fi
if [[ $("$REAL_GIT" --git-dir="$REMOTE" show "$CREATED_OID:.github/docker/build-app-cli/image.json") == "{\"digest\":\"$DIGEST\",\"image-source-revision\":\"$S\",\"provenance-protocol\":1,\"repository\":\"ghcr.io/stackpop/edgezero-build-app-cli\",\"tag\":\"$TAG\"}" ]]; then
  ok 'the typed writer installs the exact image record on the branch'
else
  no 'the typed writer installs the exact image record on the branch'
fi
if [[ $("$REAL_GIT" --git-dir="$REMOTE" show "$CREATED_OID:.github/docker/build-app-cli/image-release-evidence.json") == "$(<"$APPROVAL")" ]]; then
  ok 'the typed writer installs the exact ten-field evidence record on the branch'
else
  no 'the typed writer installs the exact ten-field evidence record on the branch'
fi
created_tree=$("$REAL_GIT" --git-dir="$REMOTE" diff-tree --no-commit-id --raw -r "$CREATED_OID")
if [[ $(printf '%s\n' "$created_tree" | awk '$1 == ":000000" && $2 == "100644" && $3 ~ /^0+$/ && $5 == "A" {count++} END {print count+0}') == 2 &&
  $(printf '%s\n' "$created_tree" | wc -l | tr -d '[:space:]') == 2 ]]; then
  ok 'the commit mutates exactly two regular mode-100644 blobs'
else
  no 'the commit mutates exactly two regular mode-100644 blobs'
fi
expected_create=$(jq -cnS --arg base main --arg body "$(expected_body "$S" "$DIGEST")" \
  --arg head "edgezero-build-container-pin/$S" --arg title "$(expected_title "$S")" \
  '{base:$base,body:$body,draft:false,head:$head,title:$title}')
if [[ $(<"$LOG_ROOT/curl-4/body") == "$expected_create" ]]; then
  ok 'POST uses the exact closed request body bytes'
else
  no 'POST uses the exact closed request body bytes'
fi
assert_request_order "$(printf '%s\n' \
  "GET https://api.github.com/users/${BOT_LOGIN%\[bot\]}%5Bbot%5D" \
  'GET https://api.github.com/repos/stackpop/edgezero' \
  'GET https://api.github.com/repos/stackpop/edgezero/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=1' \
  'POST https://api.github.com/repos/stackpop/edgezero/pulls' \
  'GET https://api.github.com/repos/stackpop/edgezero/pulls/900')" \
  'creation performs the exact REST request sequence'
assert_api_transport
if [[ ! -e "$LOG_ROOT/environment-leaks" ]]; then
  ok 'ordinary execution exports no credential alias and disables lazy Git fetching'
else
  cat "$LOG_ROOT/environment-leaks" >&2
  no 'ordinary execution exports no credential alias and disables lazy Git fetching'
fi
if tr '\0' '\n' <"$LOG_ROOT/git.argv" | rg -q -- "--force-with-lease=refs/heads/edgezero-build-container-pin/$S:" &&
  tr '\0' '\n' <"$LOG_ROOT/git.argv" | rg -q -x -- '--porcelain' &&
  tr '\0' '\n' <"$LOG_ROOT/git.argv" | rg -q -x -- '--no-verify' &&
  ! tr '\0' '\n' <"$LOG_ROOT/git.argv" | rg -Fq "$TOKEN"; then
  ok 'creation uses the empty exact lease, porcelain/no-verify, and no token argv'
else
  no 'creation uses the empty exact lease, porcelain/no-verify, and no token argv'
fi
if [[ $(sort -u "$LOG_ROOT/git-auth") == $'0\t700' ]]; then
  ok 'every remote Git command disables prompts and uses only the mode-0700 askpass helper'
else
  no 'every remote Git command disables prompts and uses only the mode-0700 askpass helper'
fi
if ! tr '\0' '\n' <"$LOG_ROOT/git.argv" | rg -q -F "refs/heads/edgezero-build-container-pin/$S:refs/remotes/origin/pin-target"; then
  ok 'an absent target branch omits the conditional target fetch'
else
  no 'an absent target branch omits the conditional target fetch'
fi
set_pin_pr 900 "$S" "$DIGEST" open false
before=$CREATED_OID
rm -rf -- "$LOG_ROOT"; mkdir -p "$LOG_ROOT"
assert_status 0 'the same source and digest rerun is idempotent' run_updater
if [[ $(remote_ref "refs/heads/edgezero-build-container-pin/$S") == "$before" ]]; then
  ok 'idempotence leaves the exact branch OID unchanged'
else
  no 'idempotence leaves the exact branch OID unchanged'
fi
assert_no_mutation 'idempotence performs no API mutation'

new_case "$G"
printf '%s' "$G" >"$CASE_ROOT/created-merge-sha"
assert_status 0 'a created open PR accepts its valid final server merge-commit identity' run_updater

new_case "$G"
printf '%s' "edgezero-build-container-pin/$S" >"$CASE_ROOT/race-create"
assert_status 1 'a branch created after the absence recheck loses the empty lease race' run_updater
assert_no_mutation 'a lease race performs no pull-request mutation'
assert_silent_sanitized 'a lease-race diagnostic is sanitized'

new_case "$G" "$S" "$DIGEST"
create_pin_branch "$S" "$OLD_DIGEST"
printf '%s\n%s\n' "edgezero-build-container-pin/$S" "$S" >"$CASE_ROOT/race-update-existing"
assert_status 1 'an existing target update loses an exact-OID lease race' run_updater
if [[ $(remote_ref "refs/heads/edgezero-build-container-pin/$S") == "$S" ]]; then
  ok 'an existing-target lease race preserves the winning remote OID'
else
  no 'an existing-target lease race preserves the winning remote OID'
fi
assert_no_mutation 'an existing-target lease race performs no pull-request mutation'

new_case "$G"
printf '%s' "edgezero-build-container-pin/$S" >"$CASE_ROOT/race-readback"
assert_status 1 'a branch moved after push is rejected by immediate remote readback' run_updater
assert_no_mutation 'a post-push readback race performs no pull-request mutation'

new_case "$G"
printf '%s' "$S" >"$CASE_ROOT/race-post-head"
assert_status 1 'a branch moved during PR creation fails final remote/API proof' run_updater
assert_silent_sanitized 'a post-create branch race diagnostic is sanitized'

new_case "$G"
printf '%s' "$S" >"$CASE_ROOT/race-final-main"
assert_status 1 'remote main moving at the final combined read is rejected' run_updater
if [[ $(remote_ref refs/heads/main) == "$S" ]]; then
  ok 'the final-main race fixture moved the protected ref'
else
  no 'the final-main race fixture moved the protected ref'
fi

new_case "$G"
create_pin_branch "$S" "$DIGEST"
set_pin_pr 41 "$S" "$DIGEST" closed false
assert_status 0 'one closed unmerged exact proposal is reopened and reconciled' run_updater
if [[ $(jq -r .state "$API_ROOT/pull-41.json") == open ]]; then
  ok 'closed-unmerged reconciliation leaves the PR open'
else
  no 'closed-unmerged reconciliation leaves the PR open'
fi
if tr '\0' '\n' <"$LOG_ROOT/git.argv" | rg -q -F \
  "refs/heads/edgezero-build-container-pin/$S:refs/remotes/origin/pin-target"; then
  ok 'an existing target fetch records and verifies its exact remote OID'
else
  no 'an existing target fetch records and verifies its exact remote OID'
fi
expected_reopen=$(jq -cnS --arg base main --arg body "$(expected_body "$S" "$DIGEST")" --arg state open --arg title "$(expected_title "$S")" '{base:$base,body:$body,state:$state,title:$title}')
if [[ $(<"$LOG_ROOT/curl-5/body") == "$expected_reopen" ]]; then ok 'reopen uses the exact reconciliation JSON bytes'; else no 'reopen uses the exact reconciliation JSON bytes'; fi

new_case "$B_EQUAL"
set_pin_pr 42 "$S" "$DIGEST" closed true
assert_status 0 'an already merged exact pin is an idempotent success' run_updater
assert_no_mutation 'an already merged exact pin performs no mutation'

new_case "$B_EQUAL"
set_pin_pr 42 "$S" "$DIGEST" closed true
printf '%s' 42 >"$CASE_ROOT/race-final-pull"
assert_status 1 'an already merged pin changed after classification is rejected' run_updater
assert_no_mutation 'an already merged final-state race is mutation-free'

new_case "$B_EQUAL"
set_pin_pr 42 "$S" "$DIGEST" closed true
jq '.merge_commit_sha="ffffffffffffffffffffffffffffffffffffffff"' "$API_ROOT/pull-42.json" >"$API_ROOT/pull-42.next"
mv "$API_ROOT/pull-42.next" "$API_ROOT/pull-42.json"
assert_status 1 'an already-merged pull with an unknown merge commit is rejected' run_updater
assert_no_mutation 'malformed merged commit identity is mutation-free'

new_case "$G" "$S" "$DIGEST"
create_pin_branch "$S" "$OLD_DIGEST"
old_recorded=$TARGET_OID
set_pin_pr 43 "$S" "$OLD_DIGEST" open false
assert_status 0 'the same source with a new digest closes and replaces the open PR' run_updater
if [[ $(jq -r .state "$API_ROOT/pull-43.json") == closed && -f "$API_ROOT/pull-900.json" ]]; then
  ok 'same-source replacement closes old before creating new'
else
  no 'same-source replacement closes old before creating new'
fi
if [[ $(<"$LOG_ROOT/curl-5/method") == PATCH && $(<"$LOG_ROOT/curl-5/body") == '{"state":"closed"}' ]]; then
  ok 'close uses the exact JSON body'
else
  no 'close uses the exact JSON body'
fi
if tr '\0' '\n' <"$LOG_ROOT/git.argv" | rg -q -- \
  "--force-with-lease=refs/heads/edgezero-build-container-pin/$S:$old_recorded"; then
  ok 'an existing target update uses its exact recorded OID lease'
else
  no 'an existing target update uses its exact recorded OID lease'
fi
replacement_oid=$(remote_ref "refs/heads/edgezero-build-container-pin/$S")
if [[ $(jq -r '.head.sha' "$API_ROOT/pull-43.json") == "$replacement_oid" ]]; then
  ok 'same-source replacement verifies the closed old PR at the post-push branch OID'
else
  no 'same-source replacement verifies the closed old PR at the post-push branch OID'
fi
rm -rf -- "$LOG_ROOT"; mkdir -p "$LOG_ROOT"
assert_status 0 'a completed same-source replacement reruns idempotently' run_updater
assert_no_mutation 'a completed same-source replacement rerun performs no mutation'

new_case "$G" "$S" "$DIGEST"
create_pin_branch "$S" "$OLD_DIGEST"
set_pin_pr 47 "$S" "$OLD_DIGEST" closed false
assert_status 0 'a closed same-source old-digest proposal is verified and replaced' run_updater
replacement_oid=$(remote_ref "refs/heads/edgezero-build-container-pin/$S")
if [[ $(jq -r '.state + " " + .head.sha' "$API_ROOT/pull-47.json") == "closed $replacement_oid" &&
  -f "$API_ROOT/pull-900.json" ]]; then
  ok 'closed same-source replacement verifies the old PR at the post-push OID'
else
  no 'closed same-source replacement verifies the old PR at the post-push OID'
fi

new_case "$N"
create_pin_branch "$S" "$DIGEST" "$G"
ancestor_target=$TARGET_OID
set_pin_pr 47 "$S" "$DIGEST" open false
assert_status 0 'an existing proposal based on an ancestor of current main is rebuilt on current main' run_updater
rebuilt_target=$(remote_ref "refs/heads/edgezero-build-container-pin/$S")
if [[ "$rebuilt_target" != "$ancestor_target" &&
  $("$REAL_GIT" --git-dir="$REMOTE" show -s --format=%P "$rebuilt_target") == "$N" &&
  $(jq -r '.state + " " + .head.sha' "$API_ROOT/pull-47.json") == "open $rebuilt_target" &&
  ! -f "$API_ROOT/pull-900.json" ]]; then
  ok 'ancestor-parent reconciliation preserves the PR and binds it to the rebuilt OID'
else
  no 'ancestor-parent reconciliation preserves the PR and binds it to the rebuilt OID'
fi

new_case "$N"
create_pin_branch "$S" "$DIGEST" "$Q"
set_pin_pr 48 "$S" "$DIGEST" open false
assert_status 1 'an existing proposal with a parent incomparable to current main is rejected' run_updater
assert_no_mutation 'incomparable target-parent rejection is API-mutation-free'

for mode_path in .github/docker/build-app-cli/image.json \
  .github/docker/build-app-cli/image-release-evidence.json; do
  new_case "$G"
  create_pin_branch "$S" "$DIGEST"
  set_pin_branch_mode "$S" "$mode_path"
  set_pin_pr 49 "$S" "$DIGEST" open false
  assert_status 1 "an existing executable $mode_path entry is rejected" run_updater
  assert_no_mutation "an invalid $mode_path mode is API-mutation-free"
done

new_case "$I"
create_pin_branch "$I" "$OLD_DIGEST" "$I"
set_pin_pr 44 "$I" "$OLD_DIGEST" open false
assert_status 0 'a forward source closes an older open proposal and creates the new one' run_updater
if [[ $(jq -r .state "$API_ROOT/pull-44.json") == closed && -f "$API_ROOT/pull-900.json" ]]; then
  ok 'older proposals close only in the successful superseding transition'
else
  no 'older proposals close only in the successful superseding transition'
fi

new_case "$I"
create_pin_branch "$I" "$OLD_DIGEST" "$I"
set_pin_pr 56 "$I" "$OLD_DIGEST" closed false
printf '%s' 56 >"$CASE_ROOT/race-final-pull"
assert_status 1 'a selected closed older proposal is re-read before success' run_updater

new_case "$B_NEWER"
assert_status 1 'a source older than the protected base pin is rejected' run_updater
assert_no_mutation 'base-pin regression is mutation-free'

new_case "$B_SIDE"
assert_status 1 'a source incomparable with the protected base pin is rejected' run_updater
assert_no_mutation 'incomparable base ancestry is mutation-free'

new_case "$N"
create_pin_branch "$N" "$DIGEST" "$N"
set_pin_pr 45 "$N" "$DIGEST" open false
before=$(remote_ref "refs/heads/edgezero-build-container-pin/$N")
assert_status 0 'a newer existing proposal supersedes an older run without mutation' run_updater
if [[ $(remote_ref "refs/heads/edgezero-build-container-pin/$N") == "$before" ]]; then
  ok 'superseded success leaves the newer branch unchanged'
else
  no 'superseded success leaves the newer branch unchanged'
fi
assert_no_mutation 'superseded success performs no API mutation'

new_case "$N"
create_pin_branch "$N" "$DIGEST" "$N"
set_pin_pr 45 "$N" "$DIGEST" open false
printf '%s' 45 >"$CASE_ROOT/race-final-pull"
assert_status 1 'a newer proposal changed after classification is rejected' run_updater
assert_no_mutation 'a superseded final-state race is mutation-free'

new_case "$B_NEWER"
create_pin_branch "$N" "$DIGEST" "$B_NEWER"
set_pin_pr 46 "$N" "$DIGEST" closed true
assert_status 1 'a stale run after a newer merge is rejected before mutation' run_updater
assert_no_mutation 'stale-after-merge rejection is mutation-free'

for collision in missing-head-repo wrong-author wrong-head-repo; do
  new_case "$G"
  create_pin_branch "$S" "$DIGEST"
  case "$collision" in
    missing-head-repo)
      set_pin_pr 50 "$S" "$DIGEST" open false
      jq '.head.repo=null' "$API_ROOT/pull-50.json" >"$API_ROOT/pull-50.next"
      mv "$API_ROOT/pull-50.next" "$API_ROOT/pull-50.json"
      ;;
    wrong-author) set_pin_pr 50 "$S" "$DIGEST" open false 999 attacker stackpop/edgezero ;;
    wrong-head-repo) set_pin_pr 50 "$S" "$DIGEST" open false "$BOT_ID" "$BOT_LOGIN" attacker/edgezero ;;
  esac
  assert_status 1 "$collision collision is rejected" run_updater
  assert_no_mutation "$collision collision is mutation-free"
done

for collision in wrong-title wrong-branch; do
  new_case "$G"
  create_pin_branch "$S" "$DIGEST"
  set_pin_pr 52 "$S" "$DIGEST" open false
  if [[ "$collision" == wrong-title ]]; then
    jq '.title="chore(actions): pin build container for attacker"' "$API_ROOT/pull-52.json" >"$API_ROOT/pull-52.next"
  else
    jq '.head.ref="attacker-branch"' "$API_ROOT/pull-52.json" >"$API_ROOT/pull-52.next"
  fi
  mv "$API_ROOT/pull-52.next" "$API_ROOT/pull-52.json"
  assert_status 1 "$collision pin identity collision is rejected" run_updater
  assert_no_mutation "$collision pin identity collision is mutation-free"
done

new_case "$G"
create_pin_branch "$S" "$DIGEST"
"$REAL_GIT" -C "$WORK/branch-work" -c user.name=attacker -c user.email=attacker@example.invalid \
  -c commit.gpgsign=false commit -q --amend --no-edit --reset-author --no-gpg-sign
TARGET_OID=$("$REAL_GIT" -C "$WORK/branch-work" rev-parse HEAD)
"$REAL_GIT" -C "$WORK/branch-work" push -q --force "$REMOTE" "HEAD:refs/heads/edgezero-build-container-pin/$S"
set_pin_pr 55 "$S" "$DIGEST" open false
assert_status 1 'a target branch commit with the wrong fixed bot identity is rejected' run_updater
assert_no_mutation 'malformed target commit identity is mutation-free'

new_case "$G"
create_mismatched_pin_branch "$S" "$P" "$OLD_DIGEST"
mismatched_target=$TARGET_OID
assert_status 1 'a target branch whose record source differs from its suffix is rejected' run_updater
if [[ $(remote_ref "refs/heads/edgezero-build-container-pin/$S") == "$mismatched_target" ]]; then
  ok 'target source/suffix mismatch is rejected before branch mutation'
else
  no 'target source/suffix mismatch is rejected before branch mutation'
fi
assert_no_mutation 'target source/suffix mismatch is rejected before API mutation'

new_case "$G"
create_pin_branch "$S" "$DIGEST"
append_pin_pr 53 "$S" "$DIGEST" open false
append_pin_pr 54 "$S" "$DIGEST" closed false
assert_status 1 'multiple pull requests for one source are rejected as ambiguous' run_updater
assert_no_mutation 'multiple same-source ambiguity is mutation-free'

new_case "$G"
printf '%s' '[{"id":1,"number":1,"title":"ordinary","head":{"ref":"ordinary"}},{"id":1,"number":2,"title":"ordinary-2","head":{"ref":"ordinary-2"}}]' >"$API_ROOT/pulls-page-1.json"
assert_status 1 'duplicate pull identities are rejected' run_updater
assert_no_mutation 'duplicate pagination is mutation-free'

new_case "$G"
printf '%s' '<https://evil.example/pulls?page=2>; rel="next"' >"$API_ROOT/link-page-1"
assert_status 1 'a response-supplied cross-origin pagination URL is rejected' run_updater
assert_no_mutation 'bad response pagination URLs are mutation-free'

new_case "$G"
jq -cn '[range(0;100) | {id:(.+1),number:(.+1),title:"ordinary",head:{ref:"ordinary"}}]' \
  >"$API_ROOT/pulls-page-1.json"
printf '%s' \
  '<https://api.github.com/repos/stackpop/edgezero/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=18446744073709551618>; rel="next"' \
  >"$API_ROOT/link-page-1"
assert_status 1 'an overflowing pagination target page is rejected' run_updater
assert_no_mutation 'overflowing pagination is mutation-free'

new_case "$G"
jq -cn '[range(0;100) | {id:(.+1),number:(.+1),title:"ordinary",head:{ref:"ordinary"}}]' \
  >"$API_ROOT/pulls-page-1.json"
printf '%s' '[{"id":101,"number":101,"title":"ordinary","head":{"ref":"ordinary"}}]' \
  >"$API_ROOT/pulls-page-2.json"
printf '%s' \
  '<https://api.github.com/repos/stackpop/edgezero/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=2>; rel="next", <https://api.github.com/repos/stackpop/edgezero/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=2>; rel="last"' \
  >"$API_ROOT/link-page-1"
assert_status 0 'an exact two-page pull inventory is completely enumerated' run_updater
assert_request_order "$(printf '%s\n' \
  "GET https://api.github.com/users/${BOT_LOGIN%\[bot\]}%5Bbot%5D" \
  'GET https://api.github.com/repos/stackpop/edgezero' \
  'GET https://api.github.com/repos/stackpop/edgezero/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=1' \
  'GET https://api.github.com/repos/stackpop/edgezero/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=2' \
  'POST https://api.github.com/repos/stackpop/edgezero/pulls' \
  'GET https://api.github.com/repos/stackpop/edgezero/pulls/900')" \
  'pagination synthesizes only the exact ordered page URLs'
assert_api_transport

new_case "$G"
jq -cn '[range(0;100) | {id:(.+1),number:(.+1),title:"ordinary",head:{ref:"ordinary"}}]' \
  >"$API_ROOT/pulls-page-1.json"
printf '%s' \
  '<https://api.github.com/repos/stackpop/edgezero/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=2>; rel="next", <https://api.github.com/repos/stackpop/edgezero/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=2>; rel="next"' \
  >"$API_ROOT/link-page-1"
assert_status 1 'duplicate pagination relations are rejected' run_updater
assert_no_mutation 'duplicate pagination relations are mutation-free'

new_case "$G"
touch "$API_ROOT/full-pages"
assert_status 1 'one hundred full synthesized pages fail as 10k truncation' run_updater
assert_no_mutation 'pagination truncation is mutation-free'

new_case "$G"
printf '%s' "GET /users/${BOT_LOGIN%\[bot\]}%5Bbot%5D" >"$CASE_ROOT/fail-next"
assert_status 1 'an API status failure is terminal' run_updater
assert_no_mutation 'an API status failure cannot mutate state'

new_case "$G"
printf '%s' 'POST 200' >"$CASE_ROOT/response-status"
assert_status 1 'pull creation rejects a non-201 success status' run_updater

new_case "$G"
create_pin_branch "$S" "$DIGEST"
set_pin_pr 57 "$S" "$DIGEST" closed false
printf '%s' 'PATCH 201' >"$CASE_ROOT/response-status"
assert_status 1 'pull reconciliation rejects a non-200 success status' run_updater

new_case "$G"
printf '%s' 2025-01-01 >"$API_ROOT/version"
assert_status 1 'an unexpected selected API version is rejected' run_updater
assert_no_mutation 'an API-version failure is mutation-free'

new_case "$G"
printf '%s' text/plain >"$API_ROOT/content-type"
assert_status 1 'a non-JSON response content type is rejected' run_updater
assert_no_mutation 'a response content-type failure is mutation-free'

new_case "$G"
printf '%s' 'application/json; charset=UTF-8' >"$API_ROOT/content-type"
assert_status 0 'the exact UTF-8 JSON media type is accepted' run_updater

for bad_media_type in 'application/problem+json' 'application/json; charset=utf-16' \
  'application/json; profile=unexpected'; do
  new_case "$G"
  printf '%s' "$bad_media_type" >"$API_ROOT/content-type"
  assert_status 1 "unsupported response media type $bad_media_type is rejected" run_updater
  assert_no_mutation "unsupported response media type $bad_media_type is mutation-free"
done

new_case "$G"
printf '{' >"$API_ROOT/user.json"
assert_status 1 'a malformed API JSON response is rejected' run_updater
assert_no_mutation 'malformed API JSON is mutation-free'

new_case "$G"
printf '%s' "{\"id\":$BOT_ID,\"login\":\"wrong-bot\",\"type\":\"Bot\",\"secret\":\"$TOKEN\"}" >"$API_ROOT/user.json"
assert_status 1 'a wrong authenticated bot identity is rejected' run_updater
assert_no_mutation 'wrong bot identity is mutation-free'
assert_silent_sanitized 'identity failure does not echo a token-bearing API body'

for bot_identity_mutation in wrong-id wrong-type; do
  new_case "$G"
  case "$bot_identity_mutation" in
    wrong-id) printf '%s' "{\"id\":9999,\"login\":\"$BOT_LOGIN\",\"type\":\"Bot\"}" >"$API_ROOT/user.json" ;;
    wrong-type) printf '%s' "{\"id\":$BOT_ID,\"login\":\"$BOT_LOGIN\",\"type\":\"User\"}" >"$API_ROOT/user.json" ;;
  esac
  assert_status 1 "$bot_identity_mutation public bot identity is rejected" run_updater
  assert_no_mutation "$bot_identity_mutation public bot identity is mutation-free"
done

new_case "$G"
printf '%s' '{"id":101,"full_name":"attacker/edgezero","private":false,"visibility":"public","default_branch":"main","owner":{"loginlogin":"stackpop"}}' >"$API_ROOT/repo.json"
assert_status 1 'a wrong authenticated repository identity is rejected' run_updater
assert_no_mutation 'wrong repository identity is mutation-free'

new_case "$G"
unsafe_token='unsafe"token'
assert_status 1 'a token that cannot be encoded in curl config is rejected before REST access' run_updater "$unsafe_token"
assert_no_network 'unsafe token bytes never reach curl'
if ! rg -Fq "$unsafe_token" "$CASE_ROOT/stdout" "$CASE_ROOT/stderr"; then ok 'unsafe token diagnostics are redacted'; else no 'unsafe token diagnostics are redacted'; fi

new_case "$G"
create_pin_branch "$S" "$DIGEST"
set_pin_pr 51 "$S" "$DIGEST" closed false
printf '%s' 'PATCH /repos/stackpop/edgezero/pulls/51' >"$CASE_ROOT/fail-next"
assert_status 1 'a reopen API failure is terminal' run_updater
assert_silent_sanitized 'API failure diagnostics redact the token'

for signal_case in 'HUP 129' 'INT 130' 'TERM 143'; do
  signal_name=${signal_case% *}
  signal_status=${signal_case#* }
  new_case "$G"
  printf '%s' "$signal_name" >"$CASE_ROOT/signal-next"
  assert_status "$signal_status" "$signal_name during a REST call exits with the signal status" run_updater
  if [[ -f "$CASE_ROOT/signal-temp-root" && ! -e "$(<"$CASE_ROOT/signal-temp-root")" ]]; then
    ok "$signal_name cleanup removes the private clone, worktree, askpass, token, and response files"
  else
    no "$signal_name cleanup removes the private clone, worktree, askpass, token, and response files"
  fi
  assert_silent_sanitized "$signal_name cleanup emits no token"
done

new_case "$G"
bad_approval="$APPROVAL"
printf '\n' >>"$bad_approval"
assert_status 1 'non-JCS approval bytes fail before App-token REST access' run_updater
assert_no_network 'approval validation precedes credentialed REST access'

new_case "$G"
RUN_DIGEST="$OLD_DIGEST"
assert_status 1 'approval source/tag/digest/protocol must match the invocation' run_updater
assert_no_network 'approval mismatch is rejected before token use'

new_case "$G"
status=$(env -i PATH="$FAKE_BIN:$PATH" LC_ALL=C bash "$GATE_ROOT/.github/docker/build-app-cli/update-image-pin-pr.sh" --unknown value >"$CASE_ROOT/stdout" 2>"$CASE_ROOT/stderr"; printf '%s' "$?") || true
if [[ "$status" == 2 && ! -f "$LOG_ROOT/curl-count" ]]; then ok 'strict CLI errors take precedence over missing credentials'; else no 'strict CLI errors take precedence over missing credentials'; fi

token_read_match=$(rg -n -m1 -F '${EDGEZERO_BUILD_CONTAINER_APP_TOKEN:-}' "$SOURCE_UPDATER" || true)
approval_compare_match=$(rg -n -m1 -F 'cmp -s "$EVIDENCE_OUTPUT" "$APPROVAL_JSON"' "$SOURCE_UPDATER" || true)
token_read_line=${token_read_match%%:*}
approval_compare_line=${approval_compare_match%%:*}
if [[ "$token_read_line" =~ ^[1-9][0-9]*$ && "$approval_compare_line" =~ ^[1-9][0-9]*$ &&
  "$token_read_line" -gt "$approval_compare_line" ]]; then
  ok 'the installation token is not read until CLI and precredential validation finish'
else
  no 'the installation token is not read until CLI and precredential validation finish'
fi

new_case "$G"
touch "$CASE_ROOT/fail-mktemp"
assert_status 1 'a runtime temporary-directory failure uses the runtime exit' run_updater
assert_no_network 'a runtime temporary-directory failure is pre-network'

new_case "$G"
printf dirty >"$REPOSITORY_ROOT/untracked"
assert_status 1 'a dirty source checkout is rejected before token access' run_updater
rm -f -- "$REPOSITORY_ROOT/untracked"
assert_no_network 'checkout validation occurs before token access'

new_case "$G"
assert_status 1 'an evidence URL with a mismatched source PR is rejected' \
  env -i PATH="$FAKE_BIN:$PATH" LC_ALL=C EDGEZERO_BUILD_CONTAINER_APP_TOKEN="$TOKEN" \
    bash "$GATE_ROOT/.github/docker/build-app-cli/update-image-pin-pr.sh" \
      --gate-root "$GATE_ROOT" --gate-sha "$G" --repository-root "$REPOSITORY_ROOT" \
      --source-revision "$S" --release-tag "$TAG" --image-digest "$DIGEST" \
      --provenance-protocol 1 --approval-json "$APPROVAL" --source-pr "$SOURCE_PR" \
      --evidence-url "https://github.com/stackpop/edgezero/pull/78#issuecomment-$COMMENT_ID" \
      --expected-bot-id "$BOT_ID" --expected-bot-login "$BOT_LOGIN"
assert_no_network 'evidence URL grammar is checked before token access'

new_case "$G"
assert_status 0 'realistic App bot identity succeeds under inherited xtrace and allexport' \
  run_updater_hostile_environment
if [[ ! -e "$LOG_ROOT/environment-leaks" &&
  $(<"$CASE_ROOT/stderr") != *"$TOKEN"* &&
  $(<"$CASE_ROOT/stderr") != *ambient-github-token* &&
  $(<"$CASE_ROOT/stderr") != *ambient-gh-token* &&
  $(<"$CASE_ROOT/stderr") != *ambient-token-alias* &&
  $(<"$CASE_ROOT/stderr") != *ambient-app-token-alias* &&
  $(<"$CASE_ROOT/stderr") != *ambient-local-token-alias* ]]; then
  ok 'xtrace, allexport, GITHUB_TOKEN, GH_TOKEN, and exported TOKEN cannot expose credentials'
else
  [[ ! -e "$LOG_ROOT/environment-leaks" ]] || cat "$LOG_ROOT/environment-leaks" >&2
  cat "$CASE_ROOT/stderr" >&2
  no 'xtrace, allexport, GITHUB_TOKEN, GH_TOKEN, and exported TOKEN cannot expose credentials'
fi

new_case "$G"
"$REAL_GIT" -C "$REPOSITORY_ROOT" config remote.origin.promisor true
assert_status 1 'a promisor source repository is rejected before network access' run_updater
"$REAL_GIT" -C "$REPOSITORY_ROOT" config --unset remote.origin.promisor
assert_no_network 'promisor rejection is pre-network'

new_case "$G"
"$REAL_GIT" -C "$GATE_ROOT" config extensions.partialClone origin
assert_status 1 'a partial-clone gate repository is rejected before network access' run_updater
"$REAL_GIT" -C "$GATE_ROOT" config --unset extensions.partialClone
assert_no_network 'partial-clone gate rejection is pre-network'

new_case "$G"
"$REAL_GIT" -C "$REPOSITORY_ROOT" config remote.origin.partialCloneFilter blob:none
assert_status 1 'a partial-clone filter in the source repository is rejected before network access' run_updater
"$REAL_GIT" -C "$REPOSITORY_ROOT" config --unset remote.origin.partialCloneFilter
assert_no_network 'partial-clone filter rejection is pre-network'

new_case "$G"
"$REAL_GIT" -C "$REPOSITORY_ROOT" config extensions.worktreeConfig true
"$REAL_GIT" -C "$REPOSITORY_ROOT" config --worktree remote.origin.promisor true
assert_status 1 'worktree-scoped promisor configuration is rejected before network access' run_updater
"$REAL_GIT" -C "$REPOSITORY_ROOT" config --worktree --unset remote.origin.promisor
"$REAL_GIT" -C "$REPOSITORY_ROOT" config --unset extensions.worktreeConfig
assert_no_network 'worktree-scoped promisor rejection is pre-network'

hidden_path=.github/docker/build-app-cli/check-image-pin.sh
for hidden_mode in assume-unchanged skip-worktree; do
  new_case "$G"
  case "$hidden_mode" in
    assume-unchanged)
      "$REAL_GIT" -C "$REPOSITORY_ROOT" update-index --assume-unchanged -- "$hidden_path"
      clear_hidden=--no-assume-unchanged
      ;;
    skip-worktree)
      "$REAL_GIT" -C "$REPOSITORY_ROOT" update-index --skip-worktree -- "$hidden_path"
      clear_hidden=--no-skip-worktree
      ;;
  esac
  printf '\n# hidden checkout mutation\n' >>"$REPOSITORY_ROOT/$hidden_path"
  assert_status 1 "$hidden_mode index state cannot hide a dirty source checkout" run_updater
  "$REAL_GIT" -C "$REPOSITORY_ROOT" update-index "$clear_hidden" -- "$hidden_path"
  "$REAL_GIT" -C "$REPOSITORY_ROOT" checkout -q HEAD -- "$hidden_path"
  assert_no_network "$hidden_mode rejection is pre-network"
done

new_case "$G"
printf '%s\n' "$S" >"$REPOSITORY_ROOT/.git/shallow"
assert_status 1 'a shallow source repository is rejected before network access' run_updater
rm -f -- "$REPOSITORY_ROOT/.git/shallow"
assert_no_network 'shallow source rejection is pre-network'

new_case "$G"
"$REAL_GIT" -C "$REPOSITORY_ROOT" config core.sparseCheckout true
assert_status 1 'a sparse source repository is rejected before network access' run_updater
"$REAL_GIT" -C "$REPOSITORY_ROOT" config --unset core.sparseCheckout
assert_no_network 'sparse source rejection is pre-network'

new_case "$G"
"$REAL_GIT" -C "$REPOSITORY_ROOT" replace "$G" "$P"
assert_status 1 'a source replacement ref is rejected before network access' run_updater
"$REAL_GIT" -C "$REPOSITORY_ROOT" replace -d "$G" >/dev/null
assert_no_network 'replacement-ref rejection is pre-network'

new_case "$G"
mkdir -p "$REPOSITORY_ROOT/.git/info"
printf '%s %s\n' "$G" "$P" >"$REPOSITORY_ROOT/.git/info/grafts"
assert_status 1 'a source graft is rejected before network access' run_updater
rm -f -- "$REPOSITORY_ROOT/.git/info/grafts"
assert_no_network 'graft rejection is pre-network'

new_case "$G"
mkdir -p "$REPOSITORY_ROOT/.git/objects/info"
printf '%s\n' "$GATE_ROOT/.git/objects" >"$REPOSITORY_ROOT/.git/objects/info/alternates"
assert_status 1 'an on-disk object alternate is rejected before network access' run_updater
rm -f -- "$REPOSITORY_ROOT/.git/objects/info/alternates"
assert_no_network 'on-disk alternate rejection is pre-network'

new_case "$G"
assert_status 1 'an environment-provided object alternate is rejected before network access' \
  run_updater_with_environment_alternate
assert_no_network 'environment alternate rejection is pre-network'

new_case "$G"
mkdir -p "$GATE_ROOT/.git/objects/info"
ln -s "$WORK/nonexistent-alternate" "$GATE_ROOT/.git/objects/info/alternates"
assert_status 1 'a dangling on-disk alternate in the gate repository is rejected' run_updater
rm -f -- "$GATE_ROOT/.git/objects/info/alternates"
assert_no_network 'dangling gate alternate rejection is pre-network'

for invalid_bot_login in edgezero-publisher 'Edgezero-publisher[bot]' 'edgezero--publisher[bot]'; do
  new_case "$G"
  status=$(capture env -i PATH="$FAKE_BIN:$PATH" LC_ALL=C \
    EDGEZERO_BUILD_CONTAINER_APP_TOKEN="$TOKEN" \
    bash "$GATE_ROOT/.github/docker/build-app-cli/update-image-pin-pr.sh" \
      --gate-root "$GATE_ROOT" --gate-sha "$G" --repository-root "$REPOSITORY_ROOT" \
      --source-revision "$RUN_SOURCE" --release-tag "$TAG" --image-digest "$RUN_DIGEST" \
      --provenance-protocol 1 --approval-json "$APPROVAL" --source-pr "$SOURCE_PR" \
      --evidence-url "https://github.com/stackpop/edgezero/pull/$SOURCE_PR#issuecomment-$COMMENT_ID" \
      --expected-bot-id "$BOT_ID" --expected-bot-login "$invalid_bot_login")
  if [[ "$status" == 1 && ! -f "$LOG_ROOT/curl-count" ]]; then
    ok "invalid App bot login $invalid_bot_login is rejected before network access"
  else
    no "invalid App bot login $invalid_bot_login is rejected before network access"
  fi
done

for missing_tool in bash env git jq curl mktemp stat chmod rm cmp install wc tr sed awk sort uniq mkdir ln \
  cat cp dirname basename; do
  new_case "$G"
  assert_status 2 "missing required tool $missing_tool preserves the tool-error exit" \
    run_updater_without_tool "$missing_tool"
  assert_no_network "missing required tool $missing_tool fails before network access"
done

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
