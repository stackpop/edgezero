#!/usr/bin/env bash
# GitHub expressions are deliberately literal fixture data.
# shellcheck disable=SC2016
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/scripts" "$tmp/.github/workflows"
cp "$root/scripts/run-actionlint.sh" "$tmp/scripts/run-actionlint.sh"
workflow="$tmp/.github/workflows/publish-build-container.yml"
cat >"$workflow" <<'YAML'
name: Test
on: push
concurrency:
  group: edgezero-build-container-publication
  cancel-in-progress: false
  queue: max
jobs:
  build:
    runs-on: ubuntu-24.04
    environment:
      name: build-container-release
      deployment: false
    steps:
      - run: echo hello
YAML
if actionlint -oneline "$workflow" >"$tmp/raw" 2>&1; then
  echo 'raw actionlint unexpectedly accepts queue; review compatibility rewrite' >&2; exit 1
fi
[[ "$(wc -l <"$tmp/raw" | tr -d ' ')" == 1 ]]
grep -q '"queue"' "$tmp/raw"
bash "$tmp/scripts/run-actionlint.sh" "$workflow"
cp "$workflow" "$tmp/valid"

reject() {
  if bash "$tmp/scripts/run-actionlint.sh" "$workflow" >"$tmp/result" 2>&1; then
    echo "accepted invalid compatibility fixture: $1" >&2; exit 1
  fi
}
for expr in '.concurrency.queue = "min"' '.concurrency.group = "different"' \
  '.concurrency.cancel-in-progress = true' '.concurrency.queue = "${{ vars.QUEUE }}"' \
  '.jobs.build.concurrency.queue = "max"'; do
  yq "$expr" "$tmp/valid" >"$workflow"
  reject "$expr"
done
cp "$tmp/valid" "$workflow"
printf '  queue: max\n' >>"$workflow"
reject misplaced
cp "$tmp/valid" "$workflow"
cp "$workflow" "$tmp/.github/workflows/unapproved.yml"
if bash "$tmp/scripts/run-actionlint.sh" "$tmp/.github/workflows/unapproved.yml" >/dev/null 2>&1; then
  echo 'queue accepted in unapproved workflow' >&2; exit 1
fi
rm "$tmp/.github/workflows/unapproved.yml"
yq '.jobs.build.steps[0].run = "echo ${{ needs.absent.outputs.x }}"' "$tmp/valid" >"$workflow"
reject unrelated-expression
grep -q "$workflow:14:" "$tmp/result" || { cat "$tmp/result" >&2; exit 1; }

workflow="$tmp/.github/workflows/build-app-cli.yml"
cat >"$workflow" <<'YAML'
name: Build
on: workflow_call
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
      - name: Bootstrap
        env:
          EDGEZERO_WORKFLOW_REPOSITORY: ${{ job.workflow_repository }}
          EDGEZERO_WORKFLOW_FILE_PATH: ${{ job.workflow_file_path }}
          EDGEZERO_WORKFLOW_REF: ${{ job.workflow_ref }}
          EDGEZERO_WORKFLOW_SHA: ${{ job.workflow_sha }}
        run: echo bootstrap
      - uses: actions/checkout@v7.0.1
        with:
          repository: stackpop/edgezero
          ref: ${{ job.workflow_sha }}
          persist-credentials: false
YAML
if actionlint -oneline "$workflow" >"$tmp/raw" 2>&1; then
  echo 'raw actionlint unexpectedly accepts job identity; review compatibility rewrite' >&2; exit 1
fi
[[ "$(wc -l <"$tmp/raw" | tr -d ' ')" == 5 ]]
[[ "$(grep -c 'property "workflow_' "$tmp/raw")" == 5 ]]
bash "$tmp/scripts/run-actionlint.sh" "$workflow"
cp "$workflow" "$tmp/producer"
for expr in '.jobs.build.steps[0].env.EDGEZERO_WORKFLOW_SHA = "${{ job.workflow_shaa }}"' \
  '.jobs.build.steps[0].env.OTHER = "${{ job.workflow_sha }}"' \
  '.jobs.build.steps[0].env.EDGEZERO_WORKFLOW_SHA = "${{ job.workflow_sha || github.sha }}"' \
  '.jobs.build.steps[1].with.repository = "attacker/repo"' \
  '.jobs.build.steps[0].run = "echo ${{ job.workflow_sha }}"'; do
  yq "$expr" "$tmp/producer" >"$workflow"
  reject "$expr"
done
cp "$tmp/producer" "$workflow"
printf 'name: Duplicate\n' >>"$workflow"
reject duplicate
cp "$tmp/producer" "$workflow"
printf 'env:\n  ONE: &replay value\n  TWO: *replay\n' >>"$workflow"
reject alias
cp "$tmp/producer" "$workflow"
printf 'env:\n  &key ONE: value\n  *key: another\n' >>"$workflow"
reject alias-key
sed 's/        env:/        env: {/; s/          EDGEZERO_WORKFLOW_SHA:.*/          EDGEZERO_WORKFLOW_SHA: "${{ job.workflow_sha }}", HIDDEN: "${{ needs.absent.outputs.value }}" }/; s/\(EDGEZERO_WORKFLOW_REPOSITORY: \)\(.*\)/\1"\2",/; s/\(EDGEZERO_WORKFLOW_FILE_PATH: \)\(.*\)/\1"\2",/; s/\(EDGEZERO_WORKFLOW_REF: \)\(.*\)/\1"\2",/' "$tmp/producer" >"$workflow"
reject flow-map-ancestor
cp "$tmp/producer" "$workflow"
cp "$tmp/valid" "$tmp/.github/workflows/publish-build-container.yml"
ln -s "$tmp/producer" "$tmp/.github/workflows/linked.yml"
if bash "$tmp/scripts/run-actionlint.sh" >"$tmp/result" 2>&1; then
  echo 'default workflow discovery skipped a symlink' >&2; exit 1
fi
grep -q 'symlink workflow' "$tmp/result"
echo 'actionlint compatibility contract passed'
