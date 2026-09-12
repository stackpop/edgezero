#!/usr/bin/env bash
# GitHub expressions below are deliberate literal fixture data.
# shellcheck disable=SC2016
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd -P)
WORKFLOW="$ROOT/.github/workflows/build-container-ci.yml"
GATE_TEST_RUNNER="$ROOT/.github/actions/deploy-core/tests/run.sh"
GATE_MANIFEST="$ROOT/.github/docker/build-app-cli/gate-paths.txt"
IMAGE_MANIFEST="$ROOT/.github/docker/build-app-cli/image-context-paths.txt"
CODEOWNERS="$ROOT/.github/CODEOWNERS"
PLAN="$ROOT/docs/superpowers/plans/2026-08-20-build-cache-container.md"
SPEC="$ROOT/docs/superpowers/specs/2026-08-20-edgezero-deploy-build-caching-design.md"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[[ -f "$WORKFLOW" && ! -L "$WORKFLOW" ]] || {
  printf 'missing regular workflow: %s\n' "$WORKFLOW" >&2
  exit 1
}

for required in "$GATE_MANIFEST" "$IMAGE_MANIFEST" "$CODEOWNERS" "$PLAN" "$SPEC"; do
  [[ -f "$required" && ! -L "$required" ]] || {
    printf 'missing regular gate contract file: %s\n' "$required" >&2
    exit 1
  }
done

for contract in "$PLAN" "$SPEC"; do
  grep -Fq 'preparatory gate `G_p`' "$contract" || {
    printf 'gate contract omits the preparatory-rotation protocol: %s\n' "$contract" >&2
    exit 1
  }
done
grep -Fq 'dynamic environment selectors' "$SPEC" || {
  printf 'spec omits the protected environment namespace contract\n' >&2
  exit 1
}

LC_ALL=C sort -cu "$GATE_MANIFEST"
LC_ALL=C sort -cu "$IMAGE_MANIFEST"
grep -Fqx '.github/CODEOWNERS' "$GATE_MANIFEST"
grep -Fqx '.github/docker/build-app-cli/gate-paths.txt' "$GATE_MANIFEST"
EXPECTED_CODEOWNERS="$WORK/CODEOWNERS.expected"
: >"$EXPECTED_CODEOWNERS"
while IFS= read -r path; do
  printf '/%s @stackpop/edgezero-build-container-gate-reviewers\n' "$path"
done <"$GATE_MANIFEST" >"$EXPECTED_CODEOWNERS"
printf '/.github/workflows/ @stackpop/edgezero-build-container-gate-reviewers\n' >>"$EXPECTED_CODEOWNERS"
cmp -s "$EXPECTED_CODEOWNERS" "$CODEOWNERS" || {
  printf 'CODEOWNERS is not the exact gate-manifest expansion plus workflow namespace\n' >&2
  exit 1
}

PLAN_MANIFEST="$WORK/plan-gate-paths.txt"
awk '
  $0 == "      The frozen manifest is:" { found = 1; next }
  found && $0 == "```text" { block = 1; next }
  block && $0 == "```" { closed = 1; exit }
  block { print }
  END { if (!found || !block || !closed) exit 1 }
' "$PLAN" >"$PLAN_MANIFEST" || {
  printf 'plan does not contain one closed frozen gate manifest block\n' >&2
  exit 1
}
cmp -s "$PLAN_MANIFEST" "$GATE_MANIFEST" || {
  printf 'plan frozen manifest differs from gate-paths.txt\n' >&2
  exit 1
}

while IFS= read -r path; do
  [[ -f "$ROOT/$path" && ! -L "$ROOT/$path" ]] || {
    printf 'gate manifest path is missing or not a regular file: %s\n' "$path" >&2
    exit 1
  }
  grep -Fqx "$path" "$GATE_MANIFEST" || exit 1
done <"$IMAGE_MANIFEST"
gate_count=$(wc -l <"$GATE_MANIFEST" | tr -d '[:space:]')
image_count=$(wc -l <"$IMAGE_MANIFEST" | tr -d '[:space:]')
((image_count < gate_count)) || {
  printf 'image-context manifest is not a strict gate-manifest subset\n' >&2
  exit 1
}

: >"$WORK/codeowners.expected"
while IFS= read -r path; do
  [[ -f "$ROOT/$path" && ! -L "$ROOT/$path" ]] || {
    printf 'gate manifest path is missing or not a regular file: %s\n' "$path" >&2
    exit 1
  }
  printf '/%s @stackpop/edgezero-build-container-gate-reviewers\n' "$path"
done <"$GATE_MANIFEST" >"$WORK/codeowners.expected"
printf '/.github/workflows/ @stackpop/edgezero-build-container-gate-reviewers\n' >>"$WORK/codeowners.expected"
cmp -s "$WORK/codeowners.expected" "$CODEOWNERS" || {
  printf 'CODEOWNERS is not the exact gate-manifest expansion plus workflow namespace\n' >&2
  exit 1
}

awk '
  $0 == "      The frozen manifest is:" { found = 1; next }
  found && $0 == "```text" { block = 1; next }
  block && $0 == "```" { complete = 1; exit }
  block { print }
  END { if (!found || !complete) exit 1 }
' "$PLAN" >"$WORK/plan-gate-paths.txt" || {
  printf 'implementation plan omits the frozen gate manifest\n' >&2
  exit 1
}
cmp -s "$WORK/plan-gate-paths.txt" "$GATE_MANIFEST" || {
  printf 'implementation plan gate manifest differs from gate-paths.txt\n' >&2
  exit 1
}

for focused_test in \
  assert-build-container-app-token.test.sh \
  assert-build-container-completion.test.sh \
  assert-build-container-context.test.sh \
  assert-build-container-dispatch-context.test.sh \
  build-container-workflows.test.sh \
  check-build-container-publisher.test.sh \
  check-doc-action-pins.test.mjs \
  check-image-pin.test.sh \
  classify-build-container-change.test.sh \
  install-actionlint.test.sh \
  install-yq.test.sh \
  release-approval-gate.test.sh \
  run-actionlint.test.sh \
  run-build-container-gate.test.sh \
  select-build-container-range.test.sh \
  stage-build-context.test.sh \
  update-image-pin-pr.test.sh \
  verify-build-container-publication.test.sh \
  verify-gate-rotation-lock.test.sh \
  verify-published-image.test.sh \
  verify-release-prerequisites.test.sh \
  verify-toolchain.test.sh \
  write-image-release-record.test.sh \
  write-publisher-prerequisite.test.sh; do
  grep -Fq "deploy-core/tests/$focused_test" "$GATE_TEST_RUNNER" || {
    printf 'central gate test runner omits %s\n' "$focused_test" >&2
    exit 1
  }
done

validate() {
  local input=$1 parsed="$WORK/parsed.json"
  yq -o=json -I=0 \
    '{"document": ., "aliases": [... | select(kind == "alias")], "duplicates": [.. | select(kind == "map") | to_entries | group_by(.key) | .[] | select(length > 1)]}' \
    "$input" >"$parsed" || return 1
  jq -e '
    def no_continuation:
      [.. | objects | select(has("continue-on-error"))] | length == 0;
    def bootstrap:
      .steps[0].name == "assert-hosted-runner-context" and
      .steps[0].shell == "bash" and
      .steps[0].env.BASH_ENV == "" and .steps[0].env.ENV == "" and
      .steps[0].env.EDGEZERO_RUNNER_ENVIRONMENT == "${{ runner.environment }}" and
      .steps[0].env.EDGEZERO_RUNNER_OS == "${{ runner.os }}" and
      .steps[0].env.EDGEZERO_RUNNER_ARCH == "${{ runner.arch }}" and
      .steps[0].env.EDGEZERO_REPOSITORY == "${{ github.repository }}" and
      .steps[0].env.EDGEZERO_WORKFLOW_REF == "${{ github.workflow_ref }}" and
      .steps[0].env.EDGEZERO_WORKFLOW_SHA == "${{ github.workflow_sha }}" and
      .steps[0].env.EDGEZERO_GITHUB_SHA == "${{ github.sha }}" and
      .steps[0].env.EDGEZERO_GATE_SHA == "${{ vars.EDGEZERO_BUILD_CONTAINER_GATE_SHA }}" and
      (.steps[0] | has("if") | not) and
      (.steps[0] | has("continue-on-error") | not) and
      (.steps[0].run | contains("EDGEZERO_RUNNER_ENVIRONMENT") and
        contains("EDGEZERO_RUNNER_OS") and contains("EDGEZERO_RUNNER_ARCH") and
        contains("[[ \"$EDGEZERO_RUNNER_ENVIRONMENT\" == github-hosted ]]") and
        contains("[[ \"$EDGEZERO_RUNNER_OS\" == Linux ]]") and
        contains("[[ \"$EDGEZERO_RUNNER_ARCH\" == X64 ]]") and
        contains("[[ \"$EDGEZERO_REPOSITORY\" == stackpop/edgezero ]]") and
        contains("[[ \"$EDGEZERO_GATE_SHA\" =~ ^[0-9a-f]{40}$ ]]") and
        startswith("set -euo pipefail\n"));
    def checkout($name; $path; $ref):
      [.steps[] | select(.name == $name)] == [{
        name: $name,
        if: "${{ success() }}",
        uses: "actions/checkout@v7.0.1",
        with: {
          repository: "stackpop/edgezero",
          ref: $ref,
          path: $path,
          "persist-credentials": false,
          "fetch-depth": 0
        }
      }];
    def stable($name; $kind):
      .jobs[$name] as $job |
      $job["runs-on"] == "ubuntu-24.04" and
      $job.if == "${{ github.event_name != '\''workflow_dispatch'\'' }}" and
      ($job | has("environment") | not) and
      ($job | has("permissions") | not) and
      $job.env == {BASH_ENV:"", ENV:""} and
      ($job | tostring | contains("secrets.") | not) and
      ($job | bootstrap) and
      ($job | checkout("checkout-active-gate"; ".edgezero-gate";
        "${{ vars.EDGEZERO_BUILD_CONTAINER_GATE_SHA }}")) and
      ($job | checkout("checkout-subject"; ".edgezero-subject"; "${{ github.sha }}")) and
      ($job.steps | map(.name)) == [
        "assert-hosted-runner-context",
        "checkout-active-gate",
        "checkout-subject",
        "setup-trusted-node",
        "prepare-trusted-documentation-tools",
        "run-protected-gate-tests",
        "select-candidate-range",
        "assert-exact-main-push-context",
        "check-documentation-references",
        "run-build-container-gate",
        "assert-terminal-completion"
      ] and
      $job.steps[6].id == "candidate_range" and $job.steps[7].id == "push_range" and
      ($job.steps[6].env == {
        EDGEZERO_EVENT_NAME:"${{ github.event_name }}",
        EDGEZERO_REPOSITORY:"${{ github.repository }}",
        EDGEZERO_SHA:"${{ github.sha }}",
        EDGEZERO_WORKFLOW_SHA:"${{ github.workflow_sha }}",
        EDGEZERO_REF:"${{ github.ref }}",
        EDGEZERO_REF_PROTECTED:"${{ github.ref_protected }}",
        EDGEZERO_GATE_SHA:"${{ vars.EDGEZERO_BUILD_CONTAINER_GATE_SHA }}"
      }) and
      ($job.steps[7].env == $job.steps[6].env) and
      ($job.steps[1:6] | all(.if == "${{ success() }}")) and
      $job.steps[6].if == "${{ success() && github.event_name != '\''push'\'' }}" and
      $job.steps[7].if == "${{ success() && github.event_name == '\''push'\'' }}" and
      ($job.steps[8:10] | all(.if == "${{ success() }}")) and
      $job.steps[10].if == "${{ always() }}" and
      ([ $job.steps[] | select(.if == "${{ always() }}") | .name ] ==
        ["assert-terminal-completion"]) and
      ([ $job.steps[] | select(.name == "assert-exact-main-push-context") ] | length == 1) and
      $job.steps[3].uses == "actions/setup-node@v6.5.0" and
      $job.steps[3].with["node-version-file"] == ".edgezero-gate/.tool-versions" and
      ($job.steps[4].run | contains("$GITHUB_WORKSPACE/.edgezero-gate/scripts/install-yq.sh") and
        contains("$GITHUB_WORKSPACE/.edgezero-gate/scripts/install-actionlint.sh") and
        contains("npm --prefix \"$GITHUB_WORKSPACE/.edgezero-gate/docs\" ci --ignore-scripts")) and
      $job.steps[5].env == {CI:"true"} and
      ($job.steps[5].run | contains("$GITHUB_WORKSPACE/.edgezero-gate/.github/actions/deploy-core/tests/run.sh")) and
      ($job.steps[6].run | contains("$GITHUB_WORKSPACE/.edgezero-gate/.github/docker/build-app-cli/select-build-container-range.sh")) and
      ($job.steps[7].run | contains("$GITHUB_WORKSPACE/.edgezero-gate/.github/docker/build-app-cli/select-build-container-range.sh")) and
      ($job.steps[8].run | contains("$GITHUB_WORKSPACE/.edgezero-gate/.github/actions/deploy-core/tests/check-doc-action-pins.sh") and
        contains("--base \"$EDGEZERO_BASE\"") and contains("--candidate \"$EDGEZERO_HEAD\"")) and
      ($job.steps[9].run | contains("$GITHUB_WORKSPACE/.edgezero-gate/.github/docker/build-app-cli/run-build-container-gate.sh") and
        contains("--kind \"$EDGEZERO_KIND\"") and
        ($job.steps[9].env.EDGEZERO_KIND == $kind)) and
      ($job.steps[10].run | contains("$GITHUB_WORKSPACE/.edgezero-gate/.github/docker/build-app-cli/assert-build-container-completion.sh")) and
      ([ $job.steps[] | .run? // empty | select(contains(".edgezero-subject/.github")) ] | length == 0);
    (.aliases == [] and .duplicates == []) and
    (.document as $w |
    ($w | no_continuation) and
    ($w.name == "Build container gate") and
    ($w["run-name"] == "${{ github.event_name == '\''workflow_dispatch'\'' && format('\''build-container-release-preflight pr={0} repo={1} sha={2}'\'', inputs.candidate-pr-number, inputs.candidate-head-repository, inputs.candidate-head-sha) || '\''Build container gate'\'' }}") and
    ($w.on | keys | sort) == ["merge_group","pull_request","push","workflow_dispatch"] and
    ($w.on.pull_request == null) and
    ($w.on.merge_group == {types:["checks_requested"]}) and
    ($w.on.push == {branches:["main"]}) and
    ($w.on.workflow_dispatch.inputs == {
      "candidate-pr-number": {description:"Candidate pull request number",required:true,type:"number"},
      "candidate-head-repository": {description:"Candidate head repository",required:true,type:"string"},
      "candidate-head-sha": {description:"Candidate full head SHA",required:true,type:"string"}
    }) and
    ([ $w.on | .. | objects | select(has("paths") or has("paths-ignore")) ] | length == 0) and
    ($w.permissions == {contents:"read",actions:"read","pull-requests":"read"}) and
    ($w.jobs | keys | sort) == ["build-container-local","build-container-pin","build-container-release-preflight"] and
    ($w | stable("build-container-local"; "local")) and
    ($w | stable("build-container-pin"; "pin")) and
    ($w.jobs["build-container-release-preflight"] as $preflight |
      $preflight.if == "${{ github.event_name == '\''workflow_dispatch'\'' }}" and
      $preflight["runs-on"] == "ubuntu-24.04" and
      $preflight.environment == {name:"build-container-release",deployment:false} and
      $preflight.env == {BASH_ENV:"",ENV:""} and
      ($preflight | has("permissions") | not) and
      ($preflight | bootstrap) and
      ($preflight.steps | map(.name)) == ["assert-hosted-runner-context","checkout-active-gate","assert-exact-g-dispatch-context","mint-publisher-probe-token","verify-publisher-probe-token"] and
      ([ $preflight.steps[] | select(.uses == "actions/checkout@v7.0.1") ] | length == 1) and
      ([ $preflight.steps[] | select(.name == "assert-exact-g-dispatch-context") ] | length == 1) and
      ([ $preflight.steps[] | select(.uses == "actions/create-github-app-token@v3.2.0") ] | length == 1) and
      $preflight.steps[2].if == "${{ success() }}" and
      $preflight.steps[2].env.EDGEZERO_WORKFLOW_REF == "${{ github.workflow_ref }}" and
      $preflight.steps[2].env.EDGEZERO_RELEASE_STATE == "${{ vars.EDGEZERO_BUILD_CONTAINER_RELEASE_STATE }}" and
      ($preflight.steps[2].run | contains("$GITHUB_WORKSPACE/.edgezero-gate/.github/docker/build-app-cli/assert-build-container-dispatch-context.sh")) and
      $preflight.steps[3].with["app-id"] == "${{ vars.EDGEZERO_BUILD_CONTAINER_APP_ID }}" and
      $preflight.steps[3].with["private-key"] == "${{ secrets.EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY }}" and
      $preflight.steps[4].env.EDGEZERO_EXPECTED_INSTALLATION_ID == "${{ vars.EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID }}" and
      ([ $preflight.steps[] | .with?.path? // empty | select(. == ".edgezero-subject") ] | length == 0)) and
    ([ $w.jobs[] | .steps[] | .uses? // empty ] |
      all(test("@v(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$"))))
  ' "$parsed" >/dev/null
}

validate "$WORKFLOW" || {
  printf 'build-container workflow violates its structural contract\n' >&2
  exit 1
}

reject_mutation() {
  local expression=$1
  yq "$expression" "$WORKFLOW" >"$WORK/candidate.yml"
  if validate "$WORK/candidate.yml"; then
    printf 'workflow contract accepted mutation: %s\n' "$expression" >&2
    exit 1
  fi
}

for mutation in \
  'del(."run-name")' \
  'del(.on.merge_group)' \
  '.on.workflow_dispatch.inputs.candidate-head-sha.required = false' \
  '.on.pull_request.paths = [".github/**"]' \
  '.permissions.contents = "write"' \
  '.jobs.build-container-local.runs-on = "ubuntu-latest"' \
  '.jobs.build-container-local.steps[0].if = "${{ success() }}"' \
  '.jobs.build-container-local.steps[0].env.EDGEZERO_RUNNER_OS = "Linux"' \
  '.jobs.build-container-local.steps[1].uses = "actions/checkout@v7"' \
  '.jobs.build-container-local.steps[1].with.persist-credentials = true' \
  '.jobs.build-container-local.steps[1].with.ref = "${{ github.sha }}"' \
  'del(.jobs.build-container-local.steps[7])' \
  '.jobs.build-container-local.steps[8].if = "${{ always() }}"' \
  '.jobs.build-container-local.steps[9].continue-on-error = true' \
  '.jobs.build-container-local.steps[10].if = "${{ success() }}"' \
  '.jobs.build-container-local.environment = "build-container-release"' \
  '.jobs.build-container-pin.env.TOKEN = "${{ secrets.PUBLISHER }}"' \
  '.jobs.build-container-pin.steps[6].env.EDGEZERO_WORKFLOW_SHA = "${{ github.sha }}"' \
  '.jobs.build-container-pin.steps[9].run |= sub(".edgezero-gate"; ".edgezero-subject")' \
  '.jobs.build-container-release-preflight.permissions.contents = "write"' \
  '.jobs.build-container-release-preflight.steps[2].env.EDGEZERO_WORKFLOW_REF = "${{ github.ref }}"' \
  'del(.jobs.build-container-release-preflight.steps[] | select(.name == "assert-exact-g-dispatch-context"))'; do
  reject_mutation "$mutation"
done

printf 'build-container workflow contract passed\n'
