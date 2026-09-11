#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2321
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SOURCE_VERIFY="$DIR/../../../docker/build-app-cli/verify-release-prerequisites.sh"

if [[ ! -x "$SOURCE_VERIFY" ]]; then
  printf 'FAIL: missing executable helper: %s\n' "$SOURCE_VERIFY" >&2
  exit 1
fi

WORK=$(mktemp -d)
WORK=$(cd -- "$WORK" && pwd -P)
trap 'rm -rf -- "$WORK"' EXIT

REAL_PATH=$PATH
POLICY_TOKEN='policy-audit-secret-value'
PACKAGE_TOKEN='package-audit-secret-value'
AUDIT_TOKEN='installation-audit-secret-value'
PROBE_TOKEN='publisher-probe-secret-value'
POLICY_LOGIN=policy-auditor
PACKAGE_LOGIN=package-auditor
POLICY_REVIEWER=policy-reviewer
ADMIN_REVIEWER=administrator-reviewer
OPERATOR=rotation-operator
BOT_LOGIN='edgezero-publisher[bot]'
APP_ID=5001
INSTALLATION_ID=5002
TEAM_ID=5003
BOT_ID=5004
REPO_ID=5005
CANDIDATE_PR=42
COMMENT_ID=9007199254740993
SMOKE_RUN_ID=7001
MERGE_RUN_ID=7002
PUSH_RUN_ID=7003
LOCK_RUN_ID=7004
RUN_ATTEMPT=2
LOCK_RUN_NUMBER=9
NOW=2026-09-10T12:00:00Z
REVIEWED_AT=2026-09-10T11:45:00Z
EXPIRES_AT=2026-09-10T13:00:00Z
CREATED_AT=2026-09-10T11:30:00Z
COMPLETED_AT=2026-09-10T12:00:00Z

pass=0
fail=0

ok() {
  printf '  \033[32mok\033[0m   %s\n' "$1"
  pass=$((pass + 1))
}

no() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1" >&2
  fail=$((fail + 1))
}

hash_file() { sha256sum "$1" | awk '{print $1}'; }
hash_bytes() { printf '%s' "$1" | sha256sum | awk '{print $1}'; }

GATE_ROOT="$WORK/gate"
FAKE_BIN="$WORK/fake-bin"
INPUT_ROOT="$WORK/inputs"
mkdir -p "$GATE_ROOT/.github/docker/build-app-cli" "$GATE_ROOT/.github/workflows" \
  "$FAKE_BIN" "$INPUT_ROOT"
cp "$SOURCE_VERIFY" "$GATE_ROOT/.github/docker/build-app-cli/verify-release-prerequisites.sh"
chmod 0755 "$GATE_ROOT/.github/docker/build-app-cli/verify-release-prerequisites.sh"
printf '%s\n' \
  .github/CODEOWNERS \
  .github/docker/build-app-cli/gate-paths.txt \
  .github/docker/build-app-cli/verify-release-prerequisites.sh \
  .github/workflows/build-container-ci.yml \
  >"$GATE_ROOT/.github/docker/build-app-cli/gate-paths.txt"
printf '%s\n' \
  '/.github/CODEOWNERS @stackpop/edgezero-build-container-gate-reviewers' \
  '/.github/docker/build-app-cli/gate-paths.txt @stackpop/edgezero-build-container-gate-reviewers' \
  '/.github/docker/build-app-cli/verify-release-prerequisites.sh @stackpop/edgezero-build-container-gate-reviewers' \
  '/.github/workflows/build-container-ci.yml @stackpop/edgezero-build-container-gate-reviewers' \
  >"$GATE_ROOT/.github/CODEOWNERS"
printf 'name: build-container-ci\n' >"$GATE_ROOT/.github/workflows/build-container-ci.yml"
git -C "$GATE_ROOT" init -q -b main
git -C "$GATE_ROOT" config user.name fixture
git -C "$GATE_ROOT" config user.email fixture@example.invalid
git -C "$GATE_ROOT" add .
git -C "$GATE_ROOT" commit -q -m gate
G=$(git -C "$GATE_ROOT" rev-parse HEAD)
printf '{"gate-sha":"%s","provenance-protocol":1,"release-tag":"build-container-v1"}' "$G" \
  >"$GATE_ROOT/.github/docker/build-app-cli/release-request.json"
git -C "$GATE_ROOT" add .github/docker/build-app-cli/release-request.json
git -C "$GATE_ROOT" commit -q -m release-request
S=$(git -C "$GATE_ROOT" rev-parse HEAD)
S_TREE=$(git -C "$GATE_ROOT" rev-parse "$S^{tree}")
CANDIDATE_BRANCH_HEAD=$(GIT_AUTHOR_DATE=2026-09-10T10:00:00Z GIT_COMMITTER_DATE=2026-09-10T10:00:00Z \
  git -C "$GATE_ROOT" commit-tree "$S_TREE" -p "$G" -m candidate-head)
CANDIDATE_HEAD=$S
MERGE_GROUP_SHA=$S
git -C "$GATE_ROOT" checkout -q --detach "$G"
printf 'gate-prime\n' >"$GATE_ROOT/.github/docker/build-app-cli/prime"
git -C "$GATE_ROOT" add .github/docker/build-app-cli/prime
git -C "$GATE_ROOT" commit -q -m gate-prime
G_PRIME=$(git -C "$GATE_ROOT" rev-parse HEAD)
Q_D=$G
Q_F=$G_PRIME
git -C "$GATE_ROOT" checkout -q --detach "$G"
VERIFY="$GATE_ROOT/.github/docker/build-app-cli/verify-release-prerequisites.sh"

POLICY_PNG="$INPUT_ROOT/policy.png"
ADMIN_PNG="$INPUT_ROOT/administrator.png"
KEY_FILE="$INPUT_ROOT/app-private-key.pem"
printf '\211PNG\r\n\032\npolicy-review' >"$POLICY_PNG"
printf '\211PNG\r\n\032\nadministrator-review' >"$ADMIN_PNG"
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$KEY_FILE" 2>/dev/null
chmod 0600 "$KEY_FILE"
POLICY_PNG_DIGEST="sha256:$(hash_file "$POLICY_PNG")"
ADMIN_PNG_DIGEST="sha256:$(hash_file "$ADMIN_PNG")"
POLICY_REVIEW="$INPUT_ROOT/policy-review.json"
printf '%s' "{\"expires-at\":\"$EXPIRES_AT\",\"organization-grants\":{\"administration\":\"write\",\"members\":\"read\",\"other-displayed\":\"none\"},\"repository-grants\":{\"actions\":\"read\",\"administration\":\"write\",\"checks\":\"read\",\"contents\":\"read\",\"environments\":\"read\",\"metadata\":\"read\",\"other-displayed\":\"none\",\"pull-requests\":\"read\",\"variables\":\"read\"},\"resource-owner\":\"stackpop\",\"reviewed-at\":\"$REVIEWED_AT\",\"reviewer-login\":\"$POLICY_REVIEWER\",\"schema-version\":1,\"screenshot-sha256\":\"$POLICY_PNG_DIGEST\",\"selected-repositories\":[\"stackpop/edgezero\"],\"subject-login\":\"$POLICY_LOGIN\",\"token-id\":\"18446744073709551615\"}" >"$POLICY_REVIEW"
BOOTSTRAP_EVIDENCE="sha256:$(printf '1%.0s' {1..64})"
BOOTSTRAP_RECORD="{\"evidence-sha256\":\"$BOOTSTRAP_EVIDENCE\",\"evidence-url\":null,\"gate-sha\":\"$G\",\"previous-value-sha256\":null,\"rotation-history\":{\"state\":\"bootstrap-no-rotation\"},\"schema-version\":2,\"source-pr\":null,\"source-revision\":null}"

cat >"$FAKE_BIN/date" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=$(cd -- "$(dirname -- "$0")" && pwd)
[[ "$#" -eq 2 && "$1" == -u && "$2" == +%s ]]
[[ "${LC_ALL:-}" == C ]]
[[ -z "${EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN+x}${EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN+x}${EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE+x}${AMBIENT_SECRET+x}${HOME+x}" ]]
cat "$fixture/now-epoch"
SH
chmod 0755 "$FAKE_BIN/date"
jq -nr --arg value "$NOW" '$value | fromdateiso8601' >"$FAKE_BIN/now-epoch"

cat >"$FAKE_BIN/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
fixture=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck disable=SC1091
source "$fixture/values"
[[ "${LC_ALL:-}" == C ]]
[[ -z "${EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN+x}${EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN+x}${EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE+x}${AMBIENT_SECRET+x}${HOME+x}${CURL_HOME+x}${XDG_CONFIG_HOME+x}${HTTPS_PROXY+x}${https_proxy+x}" ]]
expected=(--disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 --request)
for expected_arg in "${expected[@]}"; do [[ "$1" == "$expected_arg" ]]; shift; done
method=$1
shift
body_file= header_file= data_file= url=
while (($#)); do
  case "$1" in
    --output) body_file=$2; shift 2 ;;
    --dump-header) header_file=$2; shift 2 ;;
    --config) [[ "$2" == - ]]; shift 2 ;;
    --data-binary) data_file=${2#@}; shift 2 ;;
    https://api.github.com/*) url=$1; shift ;;
    *) exit 91 ;;
  esac
done
[[ -n "$body_file" && -n "$header_file" && -n "$url" ]]
config=$(cat)
[[ "$config" == *'header = "Accept: application/vnd.github+json"'* ]]
[[ "$config" == *'header = "X-GitHub-Api-Version: 2026-03-10"'* ]]
[[ "$config" == *'header = "User-Agent: edgezero-build-container-gate/1"'* ]]
authorization=$(printf '%s\n' "$config" | sed -n 's/^header = "Authorization: Bearer \(.*\)"$/\1/p')
[[ -n "$authorization" ]]
path=${url#https://api.github.com}
credential=unknown
case "$authorization" in
  "$POLICY_TOKEN") credential=policy ;;
  "$PACKAGE_TOKEN") credential=package ;;
  "$AUDIT_TOKEN") credential=installation-audit ;;
  "$PROBE_TOKEN") credential=publisher-probe ;;
  eyJ*) credential=app-jwt ;;
esac
case "$path" in
  /user|/orgs/stackpop/memberships/*|/orgs/stackpop/teams/*|/orgs/stackpop/actions/permissions|/orgs/stackpop/rulesets*|/repos/stackpop/edgezero/actions/permissions|/repos/stackpop/edgezero/immutable-releases|/users/*|/repos/stackpop/edgezero/pulls/*|/repos/stackpop/edgezero/rulesets*|/repos/stackpop/edgezero/actions/variables/*|/repos/stackpop/edgezero/environments/*|/repos/stackpop/edgezero/commits/*|/repos/stackpop/edgezero/actions/runs/*|/repos/stackpop/edgezero/actions/workflows/*|/repos/stackpop/edgezero/git/ref/heads/main|/repos/stackpop/edgezero)
    if [[ "$path" == /user || "$path" == /orgs/stackpop/memberships/* ]]; then [[ "$credential" == policy || "$credential" == package ]]; else [[ "$credential" == policy ]]; fi
    ;;
  /orgs/stackpop/packages*) [[ "$credential" == package ]] ;;
  /app|/app/installations/*) [[ "$credential" == app-jwt ]] ;;
  /installation/repositories*) [[ "$credential" == installation-audit || "$credential" == publisher-probe ]] ;;
  /installation/token) [[ "$credential" == installation-audit || "$credential" == publisher-probe ]] ;;
  *) exit 92 ;;
esac
printf '%s|%s|%s\n' "$credential" "$method" "$path" >>"$fixture/calls"
fault=$(cat "$fixture/fault")
status=200 content_type=application/json selected=2026-03-10 link= scopes= body='{}'
if [[ "$fault" == redirect && "$path" == /user ]]; then status=302; fi
if [[ "$fault" == media && "$path" == /user ]]; then content_type=text/plain; fi
if [[ "$fault" == version && "$path" == /user ]]; then selected=2022-11-28; fi
if [[ "$fault" == leaky-error && "$path" == /user ]]; then printf 'curl failed with %s %s %s %s\n' "$POLICY_TOKEN" "$PACKAGE_TOKEN" "$AUDIT_TOKEN" "$PROBE_TOKEN" >&2; exit 28; fi
case "$path" in
  /user)
    if [[ "$credential" == policy ]]; then body="{\"id\":101,\"login\":\"$POLICY_LOGIN\",\"type\":\"User\"}"; else body="{\"id\":102,\"login\":\"$PACKAGE_LOGIN\",\"type\":\"User\"}"; scopes='read:packages, read:org'; [[ "$fault" != scopes ]] || scopes='read:org, repo'; fi ;;
  /orgs/stackpop/memberships/*) body='{"role":"admin","state":"active"}'; [[ "$credential" != package ]] || scopes='read:packages, read:org' ;;
  /orgs/stackpop/teams/edgezero-build-container-releasers/memberships/*) body='{"role":"member","state":"active"}' ;;
  /orgs/stackpop/actions/permissions|/repos/stackpop/edgezero/actions/permissions) body='{"enabled_repositories":"all","sha_pinning_required":false}' ;;
  /repos/stackpop/edgezero) body="{\"archived\":false,\"default_branch\":\"main\",\"full_name\":\"stackpop/edgezero\",\"id\":$REPO_ID,\"owner\":{\"login\":\"stackpop\"}}" ;;
  /repos/stackpop/edgezero/immutable-releases) body='{"enabled":true,"enforced_by_owner":false}' ;;
  /users/*) body="{\"id\":$BOT_ID,\"login\":\"$BOT_LOGIN\",\"type\":\"Bot\"}" ;;
  /orgs/stackpop/rulesets\?per_page=100\&page=1) body='[{"id":11,"name":"edgezero-build-container-required-workflow"}]' ;;
  /orgs/stackpop/rulesets/11) body="{\"bypass_actors\":[],\"conditions\":{\"ref_name\":{\"exclude\":[],\"include\":[\"refs/heads/main\"]},\"repository_id\":{\"repository_ids\":[$REPO_ID]}},\"enforcement\":\"active\",\"id\":11,\"name\":\"edgezero-build-container-required-workflow\",\"rules\":[{\"parameters\":{\"do_not_enforce_on_create\":false,\"workflows\":[{\"path\":\".github/workflows/build-container-ci.yml\",\"repository_id\":$REPO_ID,\"sha\":\"$FINAL_GATE\"}]},\"type\":\"workflows\"}],\"source\":\"stackpop\",\"source_type\":\"Organization\",\"target\":\"branch\"}" ;;
  /repos/stackpop/edgezero/rulesets\?per_page=100\&page=1)
    body='[{"id":21},{"id":22},{"id":23},{"id":24},{"id":25},{"id":26}]'
    [[ "$fault" != pagination-missing-link ]] || body=$(jq -nc '[range(0;100)|{"id":(1000+.)}]')
    [[ "$fault" != pagination-duplicate ]] || body='[{"id":21},{"id":21}]' ;;
  /repos/stackpop/edgezero/rulesets\?per_page=100\&page=2) body='[]' ;;
  /repos/stackpop/edgezero/rulesets/21) body='{"bypass_actors":[],"conditions":{"ref_name":{"exclude":[],"include":["refs/heads/main"]}},"enforcement":"active","id":21,"name":"edgezero-build-container-main","rules":[{"parameters":{"allowed_merge_methods":["squash"],"dismiss_stale_reviews_on_push":true,"require_code_owner_review":true,"require_last_push_approval":true,"required_approving_review_count":2,"required_review_thread_resolution":true},"type":"pull_request"},{"parameters":{"check_response_timeout_minutes":60,"grouping_strategy":"ALLGREEN","max_entries_to_build":1,"max_entries_to_merge":1,"merge_method":"SQUASH","min_entries_to_merge":1,"min_entries_to_merge_wait_minutes":0},"type":"merge_queue"}],"source":"stackpop/edgezero","source_type":"Repository","target":"branch"}' ;;
  /repos/stackpop/edgezero/rulesets/22) body="{\"bypass_actors\":[{\"actor_id\":$TEAM_ID,\"actor_type\":\"Team\",\"bypass_mode\":\"always\"}],\"conditions\":{\"ref_name\":{\"exclude\":[],\"include\":[\"refs/tags/build-container-v*\"]}},\"enforcement\":\"active\",\"id\":22,\"name\":\"edgezero-build-container-tag-creation\",\"rules\":[{\"type\":\"creation\"}],\"source\":\"stackpop/edgezero\",\"source_type\":\"Repository\",\"target\":\"tag\"}" ;;
  /repos/stackpop/edgezero/rulesets/23) body='{"bypass_actors":[],"conditions":{"ref_name":{"exclude":[],"include":["refs/tags/build-container-v*"]}},"enforcement":"active","id":23,"name":"edgezero-build-container-tag-immutability","rules":[{"parameters":{"update_allows_fetch_and_merge":false},"type":"update"},{"type":"deletion"}],"source":"stackpop/edgezero","source_type":"Repository","target":"tag"}' ;;
  /repos/stackpop/edgezero/rulesets/24) body="{\"bypass_actors\":[{\"actor_id\":$TEAM_ID,\"actor_type\":\"Team\",\"bypass_mode\":\"always\"}],\"conditions\":{\"ref_name\":{\"exclude\":[],\"include\":[\"refs/tags/v*\"]}},\"enforcement\":\"active\",\"id\":24,\"name\":\"edgezero-action-version-tag-creation\",\"rules\":[{\"type\":\"creation\"}],\"source\":\"stackpop/edgezero\",\"source_type\":\"Repository\",\"target\":\"tag\"}" ;;
  /repos/stackpop/edgezero/rulesets/25) body='{"bypass_actors":[],"conditions":{"ref_name":{"exclude":[],"include":["refs/tags/v*"]}},"enforcement":"active","id":25,"name":"edgezero-action-version-tag-immutability","rules":[{"parameters":{"update_allows_fetch_and_merge":false},"type":"update"},{"type":"deletion"}],"source":"stackpop/edgezero","source_type":"Repository","target":"tag"}' ;;
  /repos/stackpop/edgezero/rulesets/26) body="{\"bypass_actors\":[{\"actor_id\":$APP_ID,\"actor_type\":\"Integration\",\"bypass_mode\":\"always\"}],\"conditions\":{\"ref_name\":{\"exclude\":[],\"include\":[\"refs/heads/edgezero-build-container-pin/*\"]}},\"enforcement\":\"active\",\"id\":26,\"name\":\"edgezero-build-container-pin-branches\",\"rules\":[{\"type\":\"creation\"},{\"parameters\":{\"update_allows_fetch_and_merge\":false},\"type\":\"update\"},{\"type\":\"deletion\"}],\"source\":\"stackpop/edgezero\",\"source_type\":\"Repository\",\"target\":\"branch\"}" ;;
  /repos/stackpop/edgezero/environments/build-container-release) body='{"deployment_branch_policy":{"custom_branch_policies":true,"protected_branches":false},"protection_rules":[{"prevent_self_review":true,"reviewers":[{"reviewer":{"id":5003,"slug":"edgezero-build-container-releasers","type":"Team"},"type":"Team"}],"type":"required_reviewers"}]}' ;;
  /repos/stackpop/edgezero/environments/build-container-release/deployment-branch-policies\?per_page=100\&page=1) body='{"branch_policies":[{"id":31,"name":"build-container-v*","type":"tag"}],"total_count":1}' ;;
  /repos/stackpop/edgezero/environments/build-container-release/deployment_protection_rules) body='{"custom_deployment_protection_rules":[],"total_count":0}' ;;
  /repos/stackpop/edgezero/environments/build-container-release/variables/EDGEZERO_BUILD_CONTAINER_APP_ID) body="{\"created_at\":\"2026-09-09T10:00:00Z\",\"name\":\"EDGEZERO_BUILD_CONTAINER_APP_ID\",\"updated_at\":\"2026-09-10T10:00:00Z\",\"value\":\"$APP_ID\"}" ;;
  /repos/stackpop/edgezero/environments/build-container-release/variables/EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID) body="{\"created_at\":\"2026-09-09T10:00:00Z\",\"name\":\"EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID\",\"updated_at\":\"2026-09-10T10:00:00Z\",\"value\":\"$INSTALLATION_ID\"}" ;;
  /repos/stackpop/edgezero/environments/build-container-release/variables/EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID) body="{\"created_at\":\"2026-09-09T10:00:00Z\",\"name\":\"EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID\",\"updated_at\":\"2026-09-10T10:00:00Z\",\"value\":\"$TEAM_ID\"}" ;;
  /repos/stackpop/edgezero/environments/build-container-release/secrets/EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY) body='{"created_at":"2026-09-09T10:00:00Z","name":"EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY","updated_at":"2026-09-10T10:00:00Z"}' ;;
  /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_GATE_SHA) body="{\"name\":\"EDGEZERO_BUILD_CONTAINER_GATE_SHA\",\"value\":\"$FINAL_GATE\"}" ;;
  /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_RELEASE_STATE) body='{"name":"EDGEZERO_BUILD_CONTAINER_RELEASE_STATE","value":"enabled"}' ;;
  /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_APP_ID) body="{\"name\":\"EDGEZERO_BUILD_CONTAINER_PUBLISHER_APP_ID\",\"value\":\"$APP_ID\"}" ;;
  /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_ID) body="{\"name\":\"EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_ID\",\"value\":\"$BOT_ID\"}" ;;
  /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_LOGIN) body="{\"name\":\"EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_LOGIN\",\"value\":\"$BOT_LOGIN\"}" ;;
  /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE) body=$(jq -nc --arg value "$(cat "$fixture/prerequisite")" '{name:"EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE",value:$value}') ;;
  /repos/stackpop/edgezero/pulls/$CANDIDATE_PR) body="{\"base\":{\"ref\":\"main\",\"repo\":{\"full_name\":\"stackpop/edgezero\"}},\"head\":{\"repo\":{\"full_name\":\"stackpop/edgezero\"},\"sha\":\"$CANDIDATE_HEAD\"},\"merge_commit_sha\":\"$SOURCE\",\"merged\":true,\"number\":$CANDIDATE_PR,\"state\":\"closed\"}" ;;
  /repos/stackpop/edgezero/commits/*/check-runs\?check_name=build-container-release-preflight\&filter=latest\&app_id=15368\&per_page=100\&page=1) body="{\"check_runs\":[{\"app\":{\"id\":15368},\"conclusion\":\"success\",\"details_url\":\"https://github.com/stackpop/edgezero/actions/runs/$SMOKE_RUN_ID\",\"head_sha\":\"$CANDIDATE_HEAD\",\"id\":81,\"name\":\"build-container-release-preflight\",\"status\":\"completed\"}],\"total_count\":1}" ;;
  /repos/stackpop/edgezero/commits/*/check-runs\?check_name=build-container-local\&filter=latest\&app_id=15368\&per_page=100\&page=1) body="{\"check_runs\":[{\"app\":{\"id\":15368},\"conclusion\":\"success\",\"head_sha\":\"$MERGE_GROUP_SHA\",\"id\":82,\"name\":\"build-container-local\",\"status\":\"completed\"}],\"total_count\":1}" ;;
  /repos/stackpop/edgezero/commits/*/check-runs\?check_name=build-container-pin\&filter=latest\&app_id=15368\&per_page=100\&page=1) body="{\"check_runs\":[{\"app\":{\"id\":15368},\"conclusion\":\"success\",\"head_sha\":\"$MERGE_GROUP_SHA\",\"id\":83,\"name\":\"build-container-pin\",\"status\":\"completed\"}],\"total_count\":1}" ;;
  /repos/stackpop/edgezero/actions/runs/$SMOKE_RUN_ID) body="{\"conclusion\":\"success\",\"created_at\":\"$CREATED_AT\",\"display_title\":\"build-container-release-preflight pr=$CANDIDATE_PR repo=stackpop/edgezero sha=$CANDIDATE_HEAD\",\"event\":\"workflow_dispatch\",\"head_sha\":\"$Q_D\",\"html_url\":\"https://github.com/stackpop/edgezero/actions/runs/$SMOKE_RUN_ID\",\"id\":$SMOKE_RUN_ID,\"path\":\".github/workflows/build-container-ci.yml\",\"run_attempt\":$RUN_ATTEMPT,\"status\":\"completed\"}" ;;
  /repos/stackpop/edgezero/actions/runs/$MERGE_RUN_ID) body="{\"conclusion\":\"success\",\"created_at\":\"$CREATED_AT\",\"event\":\"merge_group\",\"head_sha\":\"$MERGE_GROUP_SHA\",\"html_url\":\"https://github.com/stackpop/edgezero/actions/runs/$MERGE_RUN_ID\",\"id\":$MERGE_RUN_ID,\"path\":\".github/workflows/build-container-ci.yml\",\"run_attempt\":$RUN_ATTEMPT,\"status\":\"completed\"}" ;;
  /repos/stackpop/edgezero/actions/runs/$PUSH_RUN_ID) body="{\"conclusion\":\"success\",\"created_at\":\"$CREATED_AT\",\"event\":\"push\",\"head_sha\":\"$SOURCE\",\"html_url\":\"https://github.com/stackpop/edgezero/actions/runs/$PUSH_RUN_ID\",\"id\":$PUSH_RUN_ID,\"path\":\".github/workflows/build-container-ci.yml\",\"run_attempt\":$RUN_ATTEMPT,\"status\":\"completed\"}" ;;
  /repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID)
    if [[ "$(cat "$fixture/rotation-state")" == review ]]; then body="{\"actor\":{\"login\":\"rotation-actor\"},\"conclusion\":null,\"created_at\":\"$CREATED_AT\",\"event\":\"workflow_dispatch\",\"head_sha\":\"$Q_D\",\"id\":$LOCK_RUN_ID,\"path\":\".github/workflows/rotate-build-container-gate.yml@main\",\"run_attempt\":$RUN_ATTEMPT,\"run_number\":$LOCK_RUN_NUMBER,\"status\":\"in_progress\"}"; else body="{\"actor\":{\"login\":\"rotation-actor\"},\"conclusion\":\"success\",\"created_at\":\"$CREATED_AT\",\"event\":\"workflow_dispatch\",\"head_sha\":\"$Q_D\",\"id\":$LOCK_RUN_ID,\"path\":\".github/workflows/rotate-build-container-gate.yml@main\",\"run_attempt\":$RUN_ATTEMPT,\"run_number\":$LOCK_RUN_NUMBER,\"status\":\"completed\"}"; fi ;;
  /repos/stackpop/edgezero/actions/runs/$SMOKE_RUN_ID/attempts/$RUN_ATTEMPT/jobs\?per_page=100\&page=1) body="{\"jobs\":[{\"completed_at\":\"$COMPLETED_AT\",\"conclusion\":\"success\",\"head_sha\":\"$Q_D\",\"id\":91,\"name\":\"build-container-release-preflight\",\"steps\":[{\"conclusion\":\"success\",\"name\":\"assert-exact-g-dispatch-context\"}]}],\"total_count\":1}" ;;
  /repos/stackpop/edgezero/actions/runs/$MERGE_RUN_ID/attempts/$RUN_ATTEMPT/jobs\?per_page=100\&page=1) body="{\"jobs\":[{\"conclusion\":\"success\",\"head_sha\":\"$MERGE_GROUP_SHA\",\"id\":92,\"name\":\"build-container-local\",\"steps\":[]},{\"conclusion\":\"success\",\"head_sha\":\"$MERGE_GROUP_SHA\",\"id\":93,\"name\":\"build-container-pin\",\"steps\":[]}],\"total_count\":2}" ;;
  /repos/stackpop/edgezero/actions/runs/$PUSH_RUN_ID/attempts/$RUN_ATTEMPT/jobs\?per_page=100\&page=1) body="{\"jobs\":[{\"conclusion\":\"success\",\"head_sha\":\"$SOURCE\",\"id\":94,\"name\":\"build-container-local\",\"steps\":[{\"conclusion\":\"success\",\"name\":\"assert-exact-main-push-context\"}]},{\"conclusion\":\"success\",\"head_sha\":\"$SOURCE\",\"id\":95,\"name\":\"build-container-pin\",\"steps\":[{\"conclusion\":\"success\",\"name\":\"assert-exact-main-push-context\"}]}],\"total_count\":2}" ;;
  /repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID/attempts/$RUN_ATTEMPT/jobs\?per_page=100\&page=1) body="{\"jobs\":[{\"conclusion\":\"success\",\"head_sha\":\"$Q_D\",\"id\":96,\"name\":\"acquire-rotation-lock\",\"steps\":[]},{\"completed_at\":\"$COMPLETED_AT\",\"conclusion\":\"success\",\"head_sha\":\"$Q_D\",\"id\":97,\"name\":\"wait-for-rotation-review\",\"steps\":[{\"conclusion\":\"success\",\"name\":\"assert-exact-rotation-context\"}]}],\"total_count\":2}" ;;
  /repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID/approvals) body=$(jq -Rs '[{comment:.,environment_name:"build-container-gate-rotation-lock",reviewed_at:"2026-09-10T12:00:00Z",state:"approved",user:{login:"rotation-reviewer"}}]' <"$fixture/approval-comment") ;;
  /repos/stackpop/edgezero/actions/workflows/rotate-build-container-gate.yml/runs\?event=workflow_dispatch\&per_page=100\&page=1) body="{\"total_count\":1,\"workflow_runs\":[{\"created_at\":\"$CREATED_AT\",\"id\":$LOCK_RUN_ID,\"run_attempt\":$RUN_ATTEMPT,\"run_number\":$LOCK_RUN_NUMBER}]}" ;;
  /repos/stackpop/edgezero/git/ref/heads/main) body="{\"object\":{\"sha\":\"$MAIN_SHA\",\"type\":\"commit\"},\"ref\":\"refs/heads/main\"}" ;;
  /orgs/stackpop/packages\?package_type=container\&per_page=100\&page=1)
    scopes='read:packages, read:org'; body='[]'
    if [[ "$fault" == pagination-bad-last ]]; then
      body=$(jq -nc '[range(0;100) | {id:(6000 + .),name:("other-" + (.|tostring))}]')
      link='<https://api.github.com/orgs/stackpop/packages?package_type=container&per_page=100&page=2>; rel="next", <https://api.github.com/orgs/stackpop/packages?package_type=container&per_page=100&page=3>; rel="last"'
    fi ;;
  /orgs/stackpop/packages\?package_type=container\&per_page=100\&page=2) scopes='read:packages, read:org'; body='[]' ;;
  /orgs/stackpop/packages/container/edgezero-build-app-cli) scopes='read:packages, read:org'; body='{"name":"edgezero-build-app-cli","package_type":"container","repository":{"full_name":"stackpop/edgezero","id":5005},"visibility":"public"}' ;;
  /app) body="{\"id\":$APP_ID,\"slug\":\"edgezero-publisher\"}" ;;
  /app/installations/$INSTALLATION_ID) body="{\"account\":{\"login\":\"stackpop\",\"type\":\"Organization\"},\"id\":$INSTALLATION_ID,\"permissions\":{\"contents\":\"write\",\"metadata\":\"read\",\"pull_requests\":\"write\"},\"repository_selection\":\"selected\",\"suspended_at\":null}" ;;
  /app/installations/$INSTALLATION_ID/access_tokens)
    [[ "$method" == POST && -n "$data_file" ]]; mint=0; [[ ! -f "$fixture/mint-count" ]] || mint=$(cat "$fixture/mint-count"); mint=$((mint + 1)); printf '%s' "$mint" >"$fixture/mint-count"
    if [[ "$mint" -eq 1 ]]; then [[ "$(cat "$data_file")" == '{"permissions":{"metadata":"read"}}' ]]; body="{\"expires_at\":\"$EXPIRES_AT\",\"permissions\":{\"metadata\":\"read\"},\"token\":\"$AUDIT_TOKEN\"}"; else [[ "$(cat "$data_file")" == "{\"repository_ids\":[$REPO_ID],\"permissions\":{\"contents\":\"write\",\"pull_requests\":\"write\"}}" ]]; body="{\"expires_at\":\"$EXPIRES_AT\",\"permissions\":{\"contents\":\"write\",\"metadata\":\"read\",\"pull_requests\":\"write\"},\"token\":\"$PROBE_TOKEN\"}"; fi
    status=201 ;;
  /installation/repositories\?per_page=100\&page=1) body="{\"repositories\":[{\"full_name\":\"stackpop/edgezero\",\"id\":$REPO_ID}],\"total_count\":1}" ;;
  /installation/token) [[ "$method" == DELETE ]]; body=; status=204; content_type= ;;
  *) exit 93 ;;
esac
if [[ "$fault" == duplicate-key && "$path" == /user ]]; then body='{"id":101,"id":102,"login":"policy-auditor","type":"User"}'; fi
if [[ "$fault" == policy-hidden-bypass && "$path" == /repos/stackpop/edgezero/rulesets/21 ]]; then body=$(printf '%s' "$body" | jq -c 'del(.bypass_actors)'); fi
if [[ "$fault" == approval-reviewer-only && "$path" == "/repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID/approvals" ]]; then body=$(printf '%s' "$body" | jq -c 'map(.reviewer = .user | del(.user))'); fi
if [[ "$fault" == approval-comment-trailing-lf && "$path" == "/repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID/approvals" ]]; then body=$(printf '%s' "$body" | jq -c '.[0].comment += "\n"'); fi
if [[ "$fault" == approval-user-trailing-lf && "$path" == "/repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID/approvals" ]]; then body=$(printf '%s' "$body" | jq -c '.[0].user.login += "\n"'); fi
if [[ "$fault" == history-attempt-mismatch && "$path" == '/repos/stackpop/edgezero/actions/workflows/rotate-build-container-gate.yml/runs?event=workflow_dispatch&per_page=100&page=1' ]]; then body=$(printf '%s' "$body" | jq -c '.workflow_runs[0].run_attempt = 1'); fi
if [[ "$fault" == history-future-created && "$path" == '/repos/stackpop/edgezero/actions/workflows/rotate-build-container-gate.yml/runs?event=workflow_dispatch&per_page=100&page=1' ]]; then body=$(printf '%s' "$body" | jq -c '.workflow_runs[0].created_at = "2026-09-10T12:00:01Z"'); fi
if [[ "$fault" == detail-created-mismatch && "$path" == "/repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID" ]]; then body=$(printf '%s' "$body" | jq -c '.created_at = "2026-09-10T11:29:59Z"'); fi
if [[ "$fault" == rotation-job-head-mismatch && "$path" == "/repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID/attempts/$RUN_ATTEMPT/jobs?per_page=100&page=1" ]]; then body=$(printf '%s' "$body" | jq -c --arg head "$FINAL_GATE" '.jobs[].head_sha = $head'); fi
if [[ "$fault" == prerequisite-trailing-lf && "$path" == /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE ]]; then body=$(printf '%s' "$body" | jq -c '.value += "\n"'); fi
if [[ "$fault" == audit-enumeration-failure && "$credential" == installation-audit && "$path" == '/installation/repositories?per_page=100&page=1' ]]; then status=500; fi
if [[ "$fault" == probe-read-failure && "$credential" == publisher-probe && "$path" == /repos/stackpop/edgezero ]]; then status=500; fi
if [[ "$fault" == audit-permissions-broadened && "$credential" == app-jwt && "$path" == "/app/installations/$INSTALLATION_ID/access_tokens" && "$mint" -eq 1 ]]; then body=$(printf '%s' "$body" | jq -c '.permissions.contents = "read"'); fi
if [[ "$fault" == revoke-failure && "$path" == /installation/token ]]; then status=500; fi
if [[ "$fault" == repeated-json && "$path" == /user ]]; then body="$body$body"; fi
if [[ "$fault" == pagination-bad-link && "$path" == '/repos/stackpop/edgezero/rulesets?per_page=100&page=1' ]]; then body=$(jq -nc '[range(0;100)|{"id":(1000+.)}]'); link='<https://api.github.com/repos/stackpop/edgezero/rulesets?page=3&per_page=100>; rel="next"'; fi
{
  printf 'HTTP/1.1 %s status\r\n' "$status"
  printf 'X-GitHub-Api-Version-Selected: %s\r\n' "$selected"
  [[ -z "$content_type" ]] || printf 'Content-Type: %s\r\n' "$content_type"
  [[ -z "$link" ]] || printf 'Link: %s\r\n' "$link"
  [[ -z "$scopes" ]] || printf 'X-OAuth-Scopes: %s\r\n' "$scopes"
  printf '\r\n'
} >"$header_file"
printf '%s' "$body" >"$body_file"
SH
chmod 0755 "$FAKE_BIN/curl"

write_values() {
  cat >"$FAKE_BIN/values" <<EOF
POLICY_TOKEN='$POLICY_TOKEN'
PACKAGE_TOKEN='$PACKAGE_TOKEN'
AUDIT_TOKEN='$AUDIT_TOKEN'
PROBE_TOKEN='$PROBE_TOKEN'
POLICY_LOGIN='$POLICY_LOGIN'
PACKAGE_LOGIN='$PACKAGE_LOGIN'
BOT_LOGIN='$BOT_LOGIN'
APP_ID='$APP_ID'
INSTALLATION_ID='$INSTALLATION_ID'
TEAM_ID='$TEAM_ID'
BOT_ID='$BOT_ID'
REPO_ID='$REPO_ID'
CANDIDATE_PR='$CANDIDATE_PR'
CANDIDATE_HEAD='$CANDIDATE_HEAD'
SOURCE='$S'
MERGE_GROUP_SHA='$MERGE_GROUP_SHA'
SMOKE_RUN_ID='$SMOKE_RUN_ID'
MERGE_RUN_ID='$MERGE_RUN_ID'
PUSH_RUN_ID='$PUSH_RUN_ID'
LOCK_RUN_ID='$LOCK_RUN_ID'
RUN_ATTEMPT='$RUN_ATTEMPT'
LOCK_RUN_NUMBER='$LOCK_RUN_NUMBER'
CREATED_AT='$CREATED_AT'
COMPLETED_AT='$COMPLETED_AT'
Q_D='$Q_D'
FINAL_GATE='$G'
MAIN_SHA='$S'
EXPIRES_AT='$EXPIRES_AT'
EOF
}
write_values

new_case() {
  CASE_ROOT="$WORK/cases/$1"
  mkdir -p "$CASE_ROOT/output"
  chmod 0700 "$CASE_ROOT/output"
  EVIDENCE_OUT="$CASE_ROOT/output/evidence.json"
  PREREQUISITE_OUT="$CASE_ROOT/output/prerequisite.json"
  COMMENT_OUT="$CASE_ROOT/output/comment.txt"
  STDOUT="$CASE_ROOT/stdout"
  STDERR="$CASE_ROOT/stderr"
  : >"$FAKE_BIN/calls"; : >"$FAKE_BIN/fault"; : >"$FAKE_BIN/approval-comment"
  printf '%s' "$BOOTSTRAP_RECORD" >"$FAKE_BIN/prerequisite"
  printf review >"$FAKE_BIN/rotation-state"
  rm -f "$FAKE_BIN/mint-count"
  write_values
}

configuration_args() {
  CONFIGURATION_ARGS=(configuration --gate-root "$GATE_ROOT" --gate-sha "$G" --smoke-run-id "$SMOKE_RUN_ID" --smoke-run-attempt "$RUN_ATTEMPT" --expected-app-id "$APP_ID" --expected-installation-id "$INSTALLATION_ID" --expected-team-id "$TEAM_ID" --expected-bot-id "$BOT_ID" --expected-bot-login "$BOT_LOGIN" --policy-token-review-json "$POLICY_REVIEW" --policy-token-review-png "$POLICY_PNG" --administrator-bypass-png "$ADMIN_PNG" --administrator-bypass-reviewer "$ADMIN_REVIEWER" --administrator-bypass-reviewed-at "$REVIEWED_AT" --evidence-out "$EVIDENCE_OUT")
}

release_args() {
  RELEASE_ARGS=(release --gate-root "$GATE_ROOT" --gate-sha "$G" --candidate-pr "$CANDIDATE_PR" --evidence-url "https://github.com/stackpop/edgezero/pull/$CANDIDATE_PR#issuecomment-$COMMENT_ID" --source-revision "$S" --merge-group-sha "$MERGE_GROUP_SHA" --merge-group-run-id "$MERGE_RUN_ID" --merge-group-run-attempt "$RUN_ATTEMPT" --smoke-run-id "$SMOKE_RUN_ID" --smoke-run-attempt "$RUN_ATTEMPT" --push-run-id "$PUSH_RUN_ID" --push-run-attempt "$RUN_ATTEMPT" --expected-app-id "$APP_ID" --expected-installation-id "$INSTALLATION_ID" --expected-team-id "$TEAM_ID" --expected-bot-id "$BOT_ID" --expected-bot-login "$BOT_LOGIN" --package-auditor-login "$PACKAGE_LOGIN" --package-state absent --policy-token-review-json "$POLICY_REVIEW" --policy-token-review-png "$POLICY_PNG" --administrator-bypass-png "$ADMIN_PNG" --administrator-bypass-reviewer "$ADMIN_REVIEWER" --administrator-bypass-reviewed-at "$REVIEWED_AT" --evidence-out "$EVIDENCE_OUT" --publisher-prerequisite-out "$PREREQUISITE_OUT")
}

rotation_review_args() {
  ROTATION_REVIEW_ARGS=(rotation-review --gate-root "$GATE_ROOT" --old-gate-sha "$G" --new-gate-sha "$G_PRIME" --dispatch-sha "$Q_D" --final-head-sha "$Q_F" --lock-run-id "$LOCK_RUN_ID" --lock-run-attempt "$RUN_ATTEMPT" --operator-login "$OPERATOR" --result activated --policy-token-review-json "$POLICY_REVIEW" --policy-token-review-png "$POLICY_PNG" --evidence-out "$EVIDENCE_OUT" --approval-comment-out "$COMMENT_OUT")
}

rotation_complete_args() {
  ROTATION_COMPLETE_ARGS=(rotation-complete --gate-root "$GATE_ROOT" --gate-sha "$G" --lock-run-id "$LOCK_RUN_ID" --lock-run-attempt "$RUN_ATTEMPT" --policy-token-review-json "$POLICY_REVIEW" --policy-token-review-png "$POLICY_PNG" --evidence-out "$EVIDENCE_OUT" --publisher-prerequisite-out "$PREREQUISITE_OUT")
}

run_helper() {
  env PATH="$FAKE_BIN:$REAL_PATH" EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN="$POLICY_TOKEN" EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN="$PACKAGE_TOKEN" EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE="$KEY_FILE" AMBIENT_SECRET='ambient-secret-value' "$VERIFY" "$@" >"$STDOUT" 2>"$STDERR"
}

assert_success() { local description=$1; shift; if run_helper "$@"; then ok "$description"; else no "$description"; sed -n '1,5p' "$STDERR" >&2; fi; }
assert_contract_failure() { local description=$1; shift; local status=0; run_helper "$@" || status=$?; if [[ "$status" -eq 1 && ! -e "$EVIDENCE_OUT" && ! -e "$PREREQUISITE_OUT" && ! -e "$COMMENT_OUT" ]]; then ok "$description"; else no "$description (status=$status)"; fi; }
assert_usage_failure() { local description=$1; shift; local status=0; run_helper "$@" || status=$?; if [[ "$status" -eq 2 && ! -s "$FAKE_BIN/calls" && ! -e "$EVIDENCE_OUT" ]]; then ok "$description"; else no "$description (status=$status)"; fi; }

printf 'verify-release-prerequisites tests\n'

new_case configuration
configuration_args
assert_success 'configuration mode accepts the complete reviewed baseline' "${CONFIGURATION_ARGS[@]}"
[[ ! -s "$STDOUT" ]] && ok 'configuration success is silent' || no 'configuration success is silent'
jq -e --arg gate "$G" --arg head "$CANDIDATE_HEAD" --arg admin "$ADMIN_PNG_DIGEST" '.mode == "configuration" and ."gate-sha" == $gate and .environment."administrator-bypass"."candidate-head-sha" == $head and .environment."administrator-bypass"."sha256" == $admin and .policy."required-workflow-sha" == $gate and .credentials."app-probes"."publisher-probe-revoked" == true' "$EVIDENCE_OUT" >/dev/null && ok 'configuration emits canonical bound audit evidence' || no 'configuration emits canonical bound audit evidence'
[[ "$(tail -c 1 "$EVIDENCE_OUT" | od -An -tuC | tr -d ' ')" != 10 ]] && ok 'configuration evidence has no trailing LF' || no 'configuration evidence has no trailing LF'

new_case copied-helper
configuration_args
COPIED_HELPER="$CASE_ROOT/verify-release-prerequisites.sh"
cp "$VERIFY" "$COPIED_HELPER"; chmod 0755 "$COPIED_HELPER"; status=0
env PATH="$FAKE_BIN:$REAL_PATH" EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN="$POLICY_TOKEN" EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE="$KEY_FILE" "$COPIED_HELPER" "${CONFIGURATION_ARGS[@]}" >"$STDOUT" 2>"$STDERR" || status=$?
[[ "$status" -eq 1 && ! -s "$FAKE_BIN/calls" && ! -e "$EVIDENCE_OUT" ]] && ok 'auditor rejects execution outside the verified gate path before network' || no 'auditor rejects execution outside the verified gate path before network'

new_case release
release_args
assert_success 'release mode verifies queue, push, package absence, and credential probes' "${RELEASE_ARGS[@]}"
expected_previous="sha256:$(hash_bytes "$BOOTSTRAP_RECORD")"; expected_evidence="sha256:$(hash_file "$EVIDENCE_OUT")"
jq -e --arg evidence "$expected_evidence" --arg previous "$expected_previous" --arg gate "$G" --arg source "$S" --arg pr "$CANDIDATE_PR" --arg url "https://github.com/stackpop/edgezero/pull/$CANDIDATE_PR#issuecomment-$COMMENT_ID" 'keys == ["evidence-sha256","evidence-url","gate-sha","previous-value-sha256","rotation-history","schema-version","source-pr","source-revision"] and ."schema-version" == 2 and ."evidence-sha256" == $evidence and ."previous-value-sha256" == $previous and ."gate-sha" == $gate and ."source-revision" == $source and ."source-pr" == $pr and ."evidence-url" == $url and ."rotation-history" == {"state":"bootstrap-no-rotation"}' "$PREREQUISITE_OUT" >/dev/null && ok 'release emits the exact schema-version-2 source tuple' || no 'release emits the exact schema-version-2 source tuple'
[[ ! -s "$STDOUT" ]] && ok 'release success is silent' || no 'release success is silent'
grep -q '^package|GET|/orgs/stackpop/packages?package_type=container&per_page=100&page=1$' "$FAKE_BIN/calls" && grep -q '^installation-audit|DELETE|/installation/token$' "$FAKE_BIN/calls" && grep -q '^publisher-probe|DELETE|/installation/token$' "$FAKE_BIN/calls" && ok 'release routes each API surface through its exact credential' || no 'release routes each API surface through its exact credential'

new_case release-public
release_args
for i in "${!RELEASE_ARGS[@]}"; do [[ "${RELEASE_ARGS[$i]}" != --package-state ]] || RELEASE_ARGS[$((i + 1))]=public-linked; done
assert_success 'release accepts a public repository-linked package' "${RELEASE_ARGS[@]}"

new_case release-distinct-pr-head
sed -i.bak "s/CANDIDATE_HEAD='$S'/CANDIDATE_HEAD='$CANDIDATE_BRANCH_HEAD'/" "$FAKE_BIN/values"
release_args
assert_success 'release distinguishes the preflight PR head from the merged source revision' "${RELEASE_ARGS[@]}"

new_case rotation-review
sed -i.bak "s/FINAL_GATE='$G'/FINAL_GATE='$G_PRIME'/; s/MAIN_SHA='$S'/MAIN_SHA='$Q_F'/" "$FAKE_BIN/values"
rotation_review_args
assert_success 'rotation-review verifies restored policy while the lock is held' "${ROTATION_REVIEW_ARGS[@]}"
line_one=$(sed -n '1p' "$COMMENT_OUT"); line_two=$(sed -n '2p' "$COMMENT_OUT"); policy_object=${line_two#edgezero-gate-rotation-policy-v1 }; receipt_object=${line_one#edgezero-gate-rotation-v1 }
[[ "$(wc -l <"$COMMENT_OUT" | tr -d ' ')" == 1 && "$(tail -c 1 "$COMMENT_OUT" | od -An -tuC | tr -d ' ')" != 10 ]] && ok 'rotation-review emits exactly two lines without a trailing LF' || no 'rotation-review emits exactly two lines without a trailing LF'
jq -e --arg digest "sha256:$(hash_bytes "$policy_object")" --arg gate "$G_PRIME" --arg head "$Q_F" '."evidence-sha256" == $digest and ."new-gate-sha" == $gate and ."head-sha" == $head and .result == "activated"' <<<"$receipt_object" >/dev/null && ok 'rotation-review binds the policy receipt digest and final identities' || no 'rotation-review binds the policy receipt digest and final identities'
[[ "$(jq -r '."policy-sha256"' <<<"$policy_object")" == "sha256:$(hash_file "$EVIDENCE_OUT")" ]] && ok 'rotation policy line binds the opaque evidence bytes' || no 'rotation policy line binds the opaque evidence bytes'
! grep -Eq '^(package|app-jwt|installation-audit|publisher-probe)\|' "$FAKE_BIN/calls" && ok 'rotation-review uses only the policy credential' || no 'rotation-review uses only the policy credential'

new_case rotation-complete
git -C "$GATE_ROOT" checkout -q --detach "$G_PRIME"
sed -i.bak "s/FINAL_GATE='$G'/FINAL_GATE='$G_PRIME'/; s/MAIN_SHA='$S'/MAIN_SHA='$Q_F'/" "$FAKE_BIN/values"
cp "$WORK/cases/rotation-review/output/comment.txt" "$FAKE_BIN/approval-comment"
printf completed >"$FAKE_BIN/rotation-state"
rotation_complete_args
for i in "${!ROTATION_COMPLETE_ARGS[@]}"; do [[ "${ROTATION_COMPLETE_ARGS[$i]}" != --gate-sha ]] || ROTATION_COMPLETE_ARGS[$((i + 1))]=$G_PRIME; done
assert_success 'rotation-complete authenticates completed lock history' "${ROTATION_COMPLETE_ARGS[@]}"
expected_history=$(printf '[{"run-attempt":"%s","run-id":"%s","run-number":"%s"}]' "$RUN_ATTEMPT" "$LOCK_RUN_ID" "$LOCK_RUN_NUMBER")
jq -e --arg evidence "sha256:$(hash_file "$EVIDENCE_OUT")" --arg history "sha256:$(hash_bytes "$expected_history")" --arg gate "$G_PRIME" --arg id "$LOCK_RUN_ID" --arg attempt "$RUN_ATTEMPT" --arg number "$LOCK_RUN_NUMBER" '."schema-version" == 2 and ."evidence-sha256" == $evidence and ."gate-sha" == $gate and ."source-revision" == null and ."source-pr" == null and ."evidence-url" == null and ."rotation-history"."history-sha256" == $history and ."rotation-history"."run-id" == $id and ."rotation-history"."run-attempt" == $attempt and ."rotation-history"."run-number" == $number and ."rotation-history".state == "verified"' "$PREREQUISITE_OUT" >/dev/null && ok 'rotation-complete emits exact schema-version-2 verified inert state' || no 'rotation-complete emits exact schema-version-2 verified inert state'
[[ "$(grep -c 'actions/workflows/rotate-build-container-gate.yml/runs' "$FAKE_BIN/calls")" -eq 2 ]] && ok 'rotation-complete uses a two-pass fail-closed history read' || no 'rotation-complete uses a two-pass fail-closed history read'

new_case rotation-approval-reviewer-only
sed -i.bak "s/FINAL_GATE='$G'/FINAL_GATE='$G_PRIME'/; s/MAIN_SHA='$S'/MAIN_SHA='$Q_F'/" "$FAKE_BIN/values"
cp "$WORK/cases/rotation-review/output/comment.txt" "$FAKE_BIN/approval-comment"
printf completed >"$FAKE_BIN/rotation-state"
printf approval-reviewer-only >"$FAKE_BIN/fault"
rotation_complete_args
for i in "${!ROTATION_COMPLETE_ARGS[@]}"; do [[ "${ROTATION_COMPLETE_ARGS[$i]}" != --gate-sha ]] || ROTATION_COMPLETE_ARGS[$((i + 1))]=$G_PRIME; done
assert_contract_failure 'rotation-complete rejects reviewer-only workflow approval identity' "${ROTATION_COMPLETE_ARGS[@]}"

for approval_case in approval-comment-trailing-lf approval-user-trailing-lf; do
  new_case "rotation-$approval_case"
  sed -i.bak "s/FINAL_GATE='$G'/FINAL_GATE='$G_PRIME'/; s/MAIN_SHA='$S'/MAIN_SHA='$Q_F'/" "$FAKE_BIN/values"
  cp "$WORK/cases/rotation-review/output/comment.txt" "$FAKE_BIN/approval-comment"
  printf completed >"$FAKE_BIN/rotation-state"
  printf '%s' "$approval_case" >"$FAKE_BIN/fault"
  rotation_complete_args
  for i in "${!ROTATION_COMPLETE_ARGS[@]}"; do [[ "${ROTATION_COMPLETE_ARGS[$i]}" != --gate-sha ]] || ROTATION_COMPLETE_ARGS[$((i + 1))]=$G_PRIME; done
  assert_contract_failure "rotation-complete rejects $approval_case" "${ROTATION_COMPLETE_ARGS[@]}"
done

for history_case in history-attempt-mismatch history-future-created detail-created-mismatch rotation-job-head-mismatch; do
  new_case "rotation-$history_case"
  sed -i.bak "s/FINAL_GATE='$G'/FINAL_GATE='$G_PRIME'/; s/MAIN_SHA='$S'/MAIN_SHA='$Q_F'/" "$FAKE_BIN/values"
  cp "$WORK/cases/rotation-review/output/comment.txt" "$FAKE_BIN/approval-comment"
  printf completed >"$FAKE_BIN/rotation-state"
  printf '%s' "$history_case" >"$FAKE_BIN/fault"
  rotation_complete_args
  for i in "${!ROTATION_COMPLETE_ARGS[@]}"; do [[ "${ROTATION_COMPLETE_ARGS[$i]}" != --gate-sha ]] || ROTATION_COMPLETE_ARGS[$((i + 1))]=$G_PRIME; done
  assert_contract_failure "rotation-complete rejects $history_case" "${ROTATION_COMPLETE_ARGS[@]}"
done

new_case rotation-large-run-number
sed -i.bak "s/FINAL_GATE='$G'/FINAL_GATE='$G_PRIME'/; s/MAIN_SHA='$S'/MAIN_SHA='$Q_F'/; s/LOCK_RUN_NUMBER='$LOCK_RUN_NUMBER'/LOCK_RUN_NUMBER='9007199254740993'/" "$FAKE_BIN/values"
cp "$WORK/cases/rotation-review/output/comment.txt" "$FAKE_BIN/approval-comment"
printf completed >"$FAKE_BIN/rotation-state"
rotation_complete_args
for i in "${!ROTATION_COMPLETE_ARGS[@]}"; do [[ "${ROTATION_COMPLETE_ARGS[$i]}" != --gate-sha ]] || ROTATION_COMPLETE_ARGS[$((i + 1))]=$G_PRIME; done
assert_success 'rotation-complete preserves a run number above the JSON safe-integer range' "${ROTATION_COMPLETE_ARGS[@]}"
[[ "$(jq -r '."rotation-history"."run-number"' "$PREREQUISITE_OUT" 2>/dev/null)" == 9007199254740993 ]] && ok 'rotation-complete emits the exact lossless run-number string' || no 'rotation-complete emits the exact lossless run-number string'

new_case rotation-rollback
git -C "$GATE_ROOT" checkout -q --detach "$G"
sed -i.bak "s/MAIN_SHA='$S'/MAIN_SHA='$Q_F'/" "$FAKE_BIN/values"
rotation_review_args
for i in "${!ROTATION_REVIEW_ARGS[@]}"; do [[ "${ROTATION_REVIEW_ARGS[$i]}" != --result ]] || ROTATION_REVIEW_ARGS[$((i + 1))]=rolled-back; done
assert_success 'rotation-review accepts an exact old-gate rollback restoration' "${ROTATION_REVIEW_ARGS[@]}"
jq -e --arg gate "$G" '."gate-sha" == $gate and ."required-workflow-sha" == $gate' <<<"$(sed -n '2s/^edgezero-gate-rotation-policy-v1 //p' "$COMMENT_OUT")" >/dev/null && ok 'rollback receipt identifies the restored old gate' || no 'rollback receipt identifies the restored old gate'

for mode_case in missing-mode unknown-mode duplicate extra cross-mode missing-evidence bad-package empty-value; do
  new_case "cli-$mode_case"; configuration_args
  case "$mode_case" in
    missing-mode) args=() ;;
    unknown-mode) args=(other "${CONFIGURATION_ARGS[@]:1}") ;;
    duplicate) args=("${CONFIGURATION_ARGS[@]}" --gate-sha "$G") ;;
    extra) args=("${CONFIGURATION_ARGS[@]}" --unknown value) ;;
    cross-mode) args=("${CONFIGURATION_ARGS[@]}" --candidate-pr "$CANDIDATE_PR") ;;
    missing-evidence) release_args; args=("${RELEASE_ARGS[@]}"); for i in "${!args[@]}"; do if [[ "${args[$i]}" == --evidence-url ]]; then unset 'args[i]' 'args[i+1]'; break; fi; done; args=("${args[@]}") ;;
    bad-package) release_args; args=("${RELEASE_ARGS[@]}"); for i in "${!args[@]}"; do [[ "${args[$i]}" != --package-state ]] || args[$((i + 1))]=private; done ;;
    empty-value) args=("${CONFIGURATION_ARGS[@]}"); args[3]= ;;
  esac
  if [[ "$mode_case" == missing-mode ]]; then
    assert_usage_failure "CLI rejects $mode_case before API access"
  else
    assert_usage_failure "CLI rejects $mode_case before API access" "${args[@]}"
  fi
done

for credential_case in configuration-policy configuration-key release-policy release-package release-key equal-audit-tokens rotation-policy; do
  new_case "credential-$credential_case"; configuration_args; args=("${CONFIGURATION_ARGS[@]}")
  env_args=(PATH="$FAKE_BIN:$REAL_PATH" EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN="$POLICY_TOKEN" EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN="$PACKAGE_TOKEN" EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE="$KEY_FILE")
  case "$credential_case" in
    configuration-policy) env_args[1]='EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN=' ;;
    configuration-key) env_args[3]='EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE=' ;;
    release-policy) release_args; args=("${RELEASE_ARGS[@]}"); env_args[1]='EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN=' ;;
    release-package) release_args; args=("${RELEASE_ARGS[@]}"); env_args[2]='EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN=' ;;
    release-key) release_args; args=("${RELEASE_ARGS[@]}"); env_args[3]='EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE=' ;;
    equal-audit-tokens) release_args; args=("${RELEASE_ARGS[@]}"); env_args[2]="EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN=$POLICY_TOKEN" ;;
    rotation-policy) rotation_review_args; args=("${ROTATION_REVIEW_ARGS[@]}"); env_args[1]='EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN=' ;;
  esac
  status=0; env "${env_args[@]}" "$VERIFY" "${args[@]}" >"$STDOUT" 2>"$STDERR" || status=$?
  [[ "$status" -eq 1 && ! -s "$FAKE_BIN/calls" ]] && ok "mode credentials reject $credential_case before network" || no "mode credentials reject $credential_case before network"
done

for review_case in duplicate reordered extra trailing wrong-grants self-review future expired bad-digest bad-policy-png bad-admin-png same-png key-mode key-symlink; do
  new_case "input-$review_case"; configuration_args; review="$CASE_ROOT/review.json"; cp "$POLICY_REVIEW" "$review"; policy_png="$POLICY_PNG"; admin_png="$ADMIN_PNG"; key="$KEY_FILE"
  case "$review_case" in
    duplicate) sed 's/"token-id":/"token-id":"7","token-id":/' "$POLICY_REVIEW" >"$review" ;;
    reordered) jq -c '{"schema-version":."schema-version","expires-at":."expires-at","organization-grants":."organization-grants","repository-grants":."repository-grants","resource-owner":."resource-owner","reviewed-at":."reviewed-at","reviewer-login":."reviewer-login","screenshot-sha256":."screenshot-sha256","selected-repositories":."selected-repositories","subject-login":."subject-login","token-id":."token-id"}' "$POLICY_REVIEW" >"$review" ;;
    extra) jq -c '.extra=true' "$POLICY_REVIEW" >"$review" ;;
    trailing) printf '\n' >>"$review" ;;
    wrong-grants) sed 's/"checks":"read"/"checks":"write"/' "$POLICY_REVIEW" >"$review" ;;
    self-review) sed "s/\"reviewer-login\":\"$POLICY_REVIEWER\"/\"reviewer-login\":\"$POLICY_LOGIN\"/" "$POLICY_REVIEW" >"$review" ;;
    future) sed "s/$REVIEWED_AT/2026-09-10T12:00:01Z/" "$POLICY_REVIEW" >"$review" ;;
    expired) sed "s/$EXPIRES_AT/$NOW/" "$POLICY_REVIEW" >"$review" ;;
    bad-digest) sed "s/$POLICY_PNG_DIGEST/sha256:$(printf 'f%.0s' {1..64})/" "$POLICY_REVIEW" >"$review" ;;
    bad-policy-png) policy_png="$CASE_ROOT/policy.png"; printf not-a-png >"$policy_png" ;;
    bad-admin-png) admin_png="$CASE_ROOT/admin.png"; printf not-a-png >"$admin_png" ;;
    same-png) admin_png="$POLICY_PNG" ;;
    key-mode) key="$CASE_ROOT/key"; cp "$KEY_FILE" "$key"; chmod 0640 "$key" ;;
    key-symlink) key="$CASE_ROOT/key"; ln -s "$KEY_FILE" "$key" ;;
  esac
  for i in "${!CONFIGURATION_ARGS[@]}"; do case "${CONFIGURATION_ARGS[$i]}" in --policy-token-review-json) CONFIGURATION_ARGS[$((i + 1))]=$review ;; --policy-token-review-png) CONFIGURATION_ARGS[$((i + 1))]=$policy_png ;; --administrator-bypass-png) CONFIGURATION_ARGS[$((i + 1))]=$admin_png ;; esac; done
  status=0; env PATH="$FAKE_BIN:$REAL_PATH" EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN="$POLICY_TOKEN" EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE="$key" "$VERIFY" "${CONFIGURATION_ARGS[@]}" >"$STDOUT" 2>"$STDERR" || status=$?
  [[ "$status" -eq 1 && ! -s "$FAKE_BIN/calls" && ! -e "$EVIDENCE_OUT" ]] && ok "local input rejects $review_case before network" || no "local input rejects $review_case before network"
done

for api_case in redirect media version repeated-json duplicate-key scopes pagination-missing-link pagination-duplicate pagination-bad-link pagination-bad-last policy-hidden-bypass; do
  new_case "api-$api_case"; printf '%s' "$api_case" >"$FAKE_BIN/fault"
  if [[ "$api_case" == scopes || "$api_case" == pagination-bad-last ]]; then release_args; args=("${RELEASE_ARGS[@]}"); else configuration_args; args=("${CONFIGURATION_ARGS[@]}"); fi
  assert_contract_failure "API rejects $api_case" "${args[@]}"
done

new_case prerequisite-trailing-lf; printf prerequisite-trailing-lf >"$FAKE_BIN/fault"; configuration_args
assert_contract_failure 'API rejects a publisher-prerequisite value with a trailing LF' "${CONFIGURATION_ARGS[@]}"

new_case audit-cleanup; printf audit-enumeration-failure >"$FAKE_BIN/fault"; configuration_args; status=0
run_helper "${CONFIGURATION_ARGS[@]}" || status=$?
if [[ "$status" -eq 1 ]] && grep -q '^installation-audit|DELETE|/installation/token$' "$FAKE_BIN/calls" && ! grep -Fq "$AUDIT_TOKEN" "$STDOUT" "$STDERR"; then ok 'failure after metadata-audit mint revokes and scrubs the token'; else no 'failure after metadata-audit mint revokes and scrubs the token'; fi

new_case audit-response-cleanup; printf audit-permissions-broadened >"$FAKE_BIN/fault"; configuration_args; status=0
run_helper "${CONFIGURATION_ARGS[@]}" || status=$?
if [[ "$status" -eq 1 ]] && grep -q '^installation-audit|DELETE|/installation/token$' "$FAKE_BIN/calls" && ! grep -Fq "$AUDIT_TOKEN" "$STDOUT" "$STDERR"; then ok 'invalid metadata-audit response still revokes and scrubs the minted token'; else no 'invalid metadata-audit response still revokes and scrubs the minted token'; fi

new_case probe-cleanup; printf probe-read-failure >"$FAKE_BIN/fault"; configuration_args; status=0
run_helper "${CONFIGURATION_ARGS[@]}" || status=$?
if [[ "$status" -eq 1 ]] && grep -q '^publisher-probe|DELETE|/installation/token$' "$FAKE_BIN/calls" && ! grep -Fq "$PROBE_TOKEN" "$STDOUT" "$STDERR"; then ok 'failure after publisher-probe mint revokes and scrubs the token'; else no 'failure after publisher-probe mint revokes and scrubs the token'; fi

new_case revoke-failure; printf revoke-failure >"$FAKE_BIN/fault"; configuration_args
assert_contract_failure 'App token revocation failure blocks evidence publication' "${CONFIGURATION_ARGS[@]}"

new_case secret-scrub; printf leaky-error >"$FAKE_BIN/fault"; configuration_args; status=0
env PATH="$FAKE_BIN:$REAL_PATH" EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN="$POLICY_TOKEN" EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE="$KEY_FILE" AMBIENT_SECRET='ambient-secret-value' bash -x "$VERIFY" "${CONFIGURATION_ARGS[@]}" >"$STDOUT" 2>"$STDERR" || status=$?
if [[ "$status" -eq 1 ]] && ! grep -Fq "$POLICY_TOKEN" "$STDOUT" "$STDERR" && ! grep -Fq "$PACKAGE_TOKEN" "$STDOUT" "$STDERR" && ! grep -Fq 'ambient-secret-value' "$STDOUT" "$STDERR"; then ok 'diagnostics and inherited xtrace scrub every credential'; else no 'diagnostics and inherited xtrace scrub every credential'; fi

for output_case in exists nonprivate repository duplicate; do
  new_case "output-$output_case"; release_args
  case "$output_case" in
    exists) : >"$EVIDENCE_OUT" ;;
    nonprivate) chmod 0755 "$CASE_ROOT/output" ;;
    repository) EVIDENCE_OUT="$GATE_ROOT/evidence.json"; for i in "${!RELEASE_ARGS[@]}"; do [[ "${RELEASE_ARGS[$i]}" != --evidence-out ]] || RELEASE_ARGS[$((i + 1))]=$EVIDENCE_OUT; done ;;
    duplicate) PREREQUISITE_OUT=$EVIDENCE_OUT; for i in "${!RELEASE_ARGS[@]}"; do [[ "${RELEASE_ARGS[$i]}" != --publisher-prerequisite-out ]] || RELEASE_ARGS[$((i + 1))]=$PREREQUISITE_OUT; done ;;
  esac
  if [[ "$output_case" == exists ]]; then
    status=0; run_helper "${RELEASE_ARGS[@]}" || status=$?
    [[ "$status" -eq 1 && -f "$EVIDENCE_OUT" && ! -e "$PREREQUISITE_OUT" && ! -e "$COMMENT_OUT" ]] && ok "output contract rejects $output_case without replacing it" || no "output contract rejects $output_case (status=$status)"
  else
    assert_contract_failure "output contract rejects $output_case" "${RELEASE_ARGS[@]}"
  fi
done

printf '\nPassed: %d  Failed: %d\n' "$pass" "$fail"
((fail == 0))
