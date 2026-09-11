#!/usr/bin/env bash
# GitHub expressions below are deliberate literal fixture data.
# shellcheck disable=SC2016
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd -P)
CHECKER_REL=.github/docker/build-app-cli/check-build-container-publisher.sh
PUBLISH_REL=.github/workflows/publish-build-container.yml
ROTATE_REL=.github/workflows/rotate-build-container-gate.yml
CHECKER="$ROOT/$CHECKER_REL"
WORK=$(mktemp -d)
WORK=$(cd -- "$WORK" && pwd -P)
trap 'rm -rf "$WORK"' EXIT

pass=0
fail=0
ok() { pass=$((pass + 1)); printf '  ok   %s\n' "$1"; }
no() { fail=$((fail + 1)); printf '  FAIL %s\n' "$1" >&2; }

printf '== build container publisher structural checker ==\n'

for required in "$CHECKER" "$ROOT/$PUBLISH_REL" "$ROOT/$ROTATE_REL"; do
  [[ -f "$required" && ! -L "$required" ]] || {
    printf 'missing required implementation file: %s\n' "$required" >&2
    exit 1
  }
done
[[ -x "$CHECKER" ]] || {
  printf 'checker is not executable: %s\n' "$CHECKER" >&2
  exit 1
}
yq -o=json -I=0 '
  .jobs.wait.steps[] | select(.name == "assert-exact-rotation-context")
' "$ROOT/$ROTATE_REL" >"$WORK/rotation-step.json"
jq -e '.env == {
  "GITHUB_TOKEN":"${{ github.token }}",
  "EDGEZERO_OLD_GATE_SHA":"${{ needs.acquire.outputs.old-gate-sha }}",
  "EDGEZERO_DISPATCH_SHA":"${{ needs.acquire.outputs.dispatch-sha }}",
  "EDGEZERO_RUN_ID":"${{ github.run_id }}",
  "EDGEZERO_RUN_ATTEMPT":"${{ github.run_attempt }}",
  "EDGEZERO_RUN_ACTOR_LOGIN":"${{ needs.acquire.outputs.run-actor-login }}"
}' "$WORK/rotation-step.json" >/dev/null || {
  printf 'rotation lock step omits its captured context bindings\n' >&2
  exit 1
}
yq -o=json -I=0 '.jobs.build-and-verify.steps' "$ROOT/$PUBLISH_REL" >"$WORK/build-steps.json"
jq -e '
  (map(.name) | index("validate-release-request")) as $validate
  | (map(.name) | index("stage-trusted-build-context")) as $stage
  | (map(.name) | index("build-publish-and-verify")) as $publish
  | $validate != null and $stage != null and $publish != null
    and $validate < $stage and $stage < $publish
  and (.[ $validate ].run | contains("classify-build-container-change.sh")
    and contains("--base") and contains("--head")
    and contains("--kind local") and contains("mode=ordinary")
    and contains("relevant=true") and contains("release-request.json")
    and contains("EDGEZERO_RELEASE_TAG"))
  and (.[ $publish ].run | contains("docker login ghcr.io")
    and contains("trap cleanup_registry EXIT")
    and contains("--provenance=false") and contains("--sbom=false")
    and contains("--file") and contains("verify-published-image.sh"))
' "$WORK/build-steps.json" >/dev/null || {
  printf 'publisher omits pre-build isolated release-request and full gate-manifest validation\n' >&2
  exit 1
}
yq -o=json -I=0 '
  .jobs.update-pin.steps[] | select(.name == "update-pin-branch")
' "$ROOT/$PUBLISH_REL" >"$WORK/update-pin-step.json"
jq -e '
  .env.EDGEZERO_APPROVAL_JSON == "${{ steps.approval.outputs.approval-json }}"
  and (.run | contains("--approval-json \"$EDGEZERO_APPROVAL_JSON\"")
    and (contains("${{ steps.approval.outputs.approval-json }}") | not))
' "$WORK/update-pin-step.json" >/dev/null || {
  printf 'publisher interpolates approval JSON into its shell program\n' >&2
  exit 1
}
yq -o=json -I=0 '
  .jobs.acquire.steps[] | select(.name == "capture-protected-dispatch")
' "$ROOT/$ROTATE_REL" >"$WORK/rotation-capture.json"
jq -e '
  (.env | has("EDGEZERO_PUBLISHER_PREREQUISITE") | not)
  and (.run | contains("gate-paths.txt") and contains("ls-tree")
    and contains("gate_entry") and contains("dispatch_entry"))
' "$WORK/rotation-capture.json" >/dev/null || {
  printf 'rotation acquire omits complete gate-manifest validation\n' >&2
  exit 1
}

git_env() {
  env -i PATH="$PATH" LC_ALL=C HOME="$WORK/home" TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    git "$@"
}

init_repo() {
  local repo=$1
  mkdir -p "$repo/.github/docker/build-app-cli" "$repo/.github/workflows" "$WORK/home"
  cp "$CHECKER" "$repo/$CHECKER_REL"
  chmod 0755 "$repo/$CHECKER_REL"
  cp "$ROOT/$PUBLISH_REL" "$repo/$PUBLISH_REL"
  cp "$ROOT/$ROTATE_REL" "$repo/$ROTATE_REL"
  printf '%s\n' 'name: Unrelated' 'on: {push: null}' 'jobs: {}' >"$repo/.github/workflows/unrelated.yml"
  git_env -C "$repo" init -q
  git_env -C "$repo" config user.name tester
  git_env -C "$repo" config user.email tester@example.invalid
  git_env -C "$repo" add .
  git_env -C "$repo" commit -qm initial
  git_env -C "$repo" switch --detach -q HEAD
}

GATE="$WORK/gate"
SUBJECT="$WORK/subject"
init_repo "$GATE"
init_repo "$SUBJECT"
G=$(git_env -C "$GATE" rev-parse HEAD)

run_checker() {
  local candidate=$1 stdout="$WORK/stdout" stderr="$WORK/stderr"
  shift
  env -i PATH="$PATH" LC_ALL=C \
    bash "$GATE/$CHECKER_REL" \
      --gate-root "$GATE" --subject-root "$SUBJECT" --gate-sha "$G" \
      --candidate-sha "$candidate" "$@" >"$stdout" 2>"$stderr"
}

T=$(git_env -C "$SUBJECT" rev-parse HEAD)
if run_checker "$T" && [[ ! -s "$WORK/stdout" ]]; then
  ok 'accepts the reviewed publisher and rotation topology without stdout'
else
  no 'accepts the reviewed publisher and rotation topology without stdout'
  sed -n '1,20p' "$WORK/stderr" >&2
fi

if run_checker "$T" --gate-sha "$G"; then
  no 'rejects duplicate flags'
else
  ok 'rejects duplicate flags'
fi
if run_checker "$T" --unknown x; then
  no 'rejects unknown flags'
else
  ok 'rejects unknown flags'
fi

commit_mutation() {
  local file=$1 expression=$2
  yq "$expression" "$SUBJECT/$file" >"$WORK/mutated.yml"
  cp "$WORK/mutated.yml" "$SUBJECT/$file"
  git_env -C "$SUBJECT" add "$file"
  git_env -C "$SUBJECT" commit -qm mutation
  git_env -C "$SUBJECT" rev-parse HEAD
}

restore_subject() {
  git_env -C "$SUBJECT" reset --hard -q "$T"
}

reject_mutation() {
  local description=$1 file=$2 expression=$3 candidate
  candidate=$(commit_mutation "$file" "$expression")
  if run_checker "$candidate"; then no "$description"; else ok "$description"; fi
  restore_subject
}

reject_mutation 'rejects a publisher branch trigger' "$PUBLISH_REL" '.on.push.branches = ["main"]'
reject_mutation 'rejects a different publication concurrency group' "$PUBLISH_REL" '.concurrency.group = "other"'
reject_mutation 'rejects cancellation of an active publisher' "$PUBLISH_REL" '.concurrency.cancel-in-progress = true'
reject_mutation 'rejects a non-FIFO concurrency queue' "$PUBLISH_REL" '.concurrency.queue = "replace"'
reject_mutation 'rejects a dynamic publisher runner label' "$PUBLISH_REL" '.jobs.build-and-verify.runs-on = "${{ matrix.runner }}"'
reject_mutation 'rejects a skipped first publisher guard' "$PUBLISH_REL" '.jobs.build-and-verify.steps[0].if = "${{ success() }}"'
reject_mutation 'rejects inherited BASH_ENV at the first guard' "$PUBLISH_REL" 'del(.jobs.build-and-verify.steps[0].env.BASH_ENV)'
reject_mutation 'rejects a writable contents grant in the build job' "$PUBLISH_REL" '.jobs.build-and-verify.permissions.contents = "write"'
reject_mutation 'rejects package deletion authority' "$PUBLISH_REL" '.jobs.build-and-verify.permissions.delete-packages = "write"'
reject_mutation 'rejects exposing the protected environment to the build job' "$PUBLISH_REL" '.jobs.build-and-verify.environment = "build-container-release"'
reject_mutation 'rejects an unpinned checkout action' "$PUBLISH_REL" '.jobs.build-and-verify.steps[1].uses = "actions/checkout@v7"'
reject_mutation 'rejects a candidate-controlled gate checkout' "$PUBLISH_REL" '.jobs.build-and-verify.steps[1].with.ref = "${{ github.sha }}"'
reject_mutation 'rejects omitted release-request classification' "$PUBLISH_REL" '.jobs.build-and-verify.steps[] |= (select(.name == "validate-release-request") .run = "set -euo pipefail\ntrue")'
reject_mutation 'rejects provenance-enabled publication builds' "$PUBLISH_REL" '.jobs.build-and-verify.steps[] |= (select(.name == "build-publish-and-verify") .run |= sub("--provenance=false"; "--provenance=true"))'
reject_mutation 'rejects publication without credential cleanup trap' "$PUBLISH_REL" '.jobs.build-and-verify.steps[] |= (select(.name == "build-publish-and-verify") .run |= sub("trap cleanup_registry EXIT"; "true"))'
reject_mutation 'rejects commands prepended to a credential-bearing run block' "$PUBLISH_REL" '.jobs.build-and-verify.steps[] |= (select(.name == "build-publish-and-verify") .run = "printf leak >&2\n" + .run)'
reject_mutation 'rejects persistent checkout credentials' "$PUBLISH_REL" '.jobs.update-pin.steps[1].with.persist-credentials = true'
reject_mutation 'rejects extra token-action inputs' "$PUBLISH_REL" '.jobs.update-pin.steps[] |= (select(.name == "mint-publisher-token") .with.skip-token-revoke = true)'
reject_mutation 'rejects extra token-action permission scopes' "$PUBLISH_REL" '.jobs.update-pin.steps[] |= (select(.name == "mint-publisher-token") .with.permission-issues = "write")'
reject_mutation 'rejects extra updater environment values' "$PUBLISH_REL" '.jobs.update-pin.steps[] |= (select(.name == "update-pin-branch") .env.EXTRA = "candidate")'
reject_mutation 'rejects updater execution from the subject checkout' "$PUBLISH_REL" '.jobs.update-pin.steps[] |= (select(.name == "update-pin-branch") .run |= sub(".edgezero-gate"; ".edgezero-source"))'
reject_mutation 'rejects token minting before approval verification' "$PUBLISH_REL" '.jobs.update-pin.steps |= ([.[0], .[1], .[2], .[3], .[4], .[6], .[5]] + .[7:])'
reject_mutation 'rejects an always-run non-cleanup publisher step' "$PUBLISH_REL" '.jobs.update-pin.steps += [{"name":"masked","if":"${{ always() }}","run":"true"}]'

reject_mutation 'rejects a rotation push trigger' "$ROTATE_REL" '.on.push = null'
reject_mutation 'rejects a different rotation concurrency group' "$ROTATE_REL" '.concurrency.group = "other"'
reject_mutation 'rejects a late rotation guard' "$ROTATE_REL" '.jobs.acquire.steps |= [.[1], .[0]]'
reject_mutation 'rejects a dynamic rotation runner label' "$ROTATE_REL" '.jobs.wait.runs-on = "ubuntu-latest"'
reject_mutation 'rejects secret access in the acquire job' "$ROTATE_REL" '.jobs.acquire.env.LEAK = "${{ secrets.KEY }}"'
reject_mutation 'rejects omitted dispatch gate-manifest comparison' "$ROTATE_REL" '.jobs.acquire.steps[] |= (select(.name == "capture-protected-dispatch") .run = "set -euo pipefail\ntrue")'
reject_mutation 'rejects rotation helper execution from a candidate root' "$ROTATE_REL" '.jobs.wait.steps[] |= (select(.name == "assert-exact-rotation-context") .run |= sub(".edgezero-gate"; ".edgezero-subject"))'
reject_mutation 'rejects a conditional exact rotation assertion' "$ROTATE_REL" '.jobs.wait.steps[] |= (select(.name == "assert-exact-rotation-context") .if = "${{ success() }}")'

printf '%s\n' \
  'name: Collision' \
  'on: {workflow_dispatch: null}' \
  'concurrency:' \
  '  group: edgezero-build-container-publication' \
  '  cancel-in-progress: false' \
  '  queue: max' \
  'jobs: {}' >"$SUBJECT/.github/workflows/collision.yml"
git_env -C "$SUBJECT" add .github/workflows/collision.yml
git_env -C "$SUBJECT" commit -qm collision
COLLISION=$(git_env -C "$SUBJECT" rev-parse HEAD)
if run_checker "$COLLISION"; then no 'rejects a third workflow claiming the shared group'; else ok 'rejects a third workflow claiming the shared group'; fi
restore_subject

printf '%s\n' \
  'name: Case collision' \
  'on: {workflow_dispatch: null}' \
  'concurrency:' \
  '  group: EdgeZero-Build-Container-Publication' \
  '  cancel-in-progress: false' \
  '  queue: max' \
  'jobs: {}' >"$SUBJECT/.github/workflows/collision.yml"
git_env -C "$SUBJECT" add .github/workflows/collision.yml
git_env -C "$SUBJECT" commit -qm case-collision
CASE_COLLISION=$(git_env -C "$SUBJECT" rev-parse HEAD)
if run_checker "$CASE_COLLISION"; then no 'rejects a case-variant concurrency collision'; else ok 'rejects a case-variant concurrency collision'; fi
restore_subject

printf '%s\n' \
  'name: Dynamic collision' \
  'on: {workflow_dispatch: null}' \
  'concurrency:' \
  "  group: \${{ 'edgezero-build-container-publication' }}" \
  '  cancel-in-progress: true' \
  'jobs: {}' >"$SUBJECT/.github/workflows/collision.yml"
git_env -C "$SUBJECT" add .github/workflows/collision.yml
git_env -C "$SUBJECT" commit -qm dynamic-collision
DYNAMIC_COLLISION=$(git_env -C "$SUBJECT" rev-parse HEAD)
if run_checker "$DYNAMIC_COLLISION"; then no 'rejects a dynamically equivalent concurrency collision'; else ok 'rejects a dynamically equivalent concurrency collision'; fi
restore_subject

printf '%s\n' \
  'name: Job collision' \
  'on: {workflow_dispatch: null}' \
  'jobs:' \
  '  collide:' \
  '    runs-on: ubuntu-24.04' \
  '    concurrency:' \
  '      group: edgezero-build-container-publication' \
  '      cancel-in-progress: true' \
  '    steps: []' >"$SUBJECT/.github/workflows/collision.yml"
git_env -C "$SUBJECT" add .github/workflows/collision.yml
git_env -C "$SUBJECT" commit -qm job-collision
JOB_COLLISION=$(git_env -C "$SUBJECT" rev-parse HEAD)
if run_checker "$JOB_COLLISION"; then no 'rejects a job-level concurrency collision'; else ok 'rejects a job-level concurrency collision'; fi
restore_subject

ln -s unrelated.yml "$SUBJECT/.github/workflows/linked.yml"
git_env -C "$SUBJECT" add .github/workflows/linked.yml
git_env -C "$SUBJECT" commit -qm linked
LINKED=$(git_env -C "$SUBJECT" rev-parse HEAD)
if run_checker "$LINKED"; then no 'rejects linked candidate workflow blobs'; else ok 'rejects linked candidate workflow blobs'; fi
restore_subject

SHARED_GIT="$WORK/shared-git"
cp -R "$GATE/.git" "$SHARED_GIT"
mv "$GATE/.git" "$WORK/gate-git"
mv "$SUBJECT/.git" "$WORK/subject-git"
ln -s "$SHARED_GIT" "$GATE/.git"
ln -s "$SHARED_GIT" "$SUBJECT/.git"
if run_checker "$G"; then no 'rejects symlinked shared Git storage'; else ok 'rejects symlinked shared Git storage'; fi

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
