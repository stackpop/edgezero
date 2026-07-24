#!/usr/bin/env bash
set -euo pipefail

# Asserts the exact Fastly call sequence a STAGED deploy through the
# deploy-fastly wrapper must produce, and that the staged version threaded out
# of the action:
#   * `--comment` must NOT reach `fastly compute update` (it has no such flag);
#     it is applied via `fastly service-version update --comment` BEFORE the
#     version is staged.
#   * `--non-interactive` is supplied as an action-owned passthrough arg, so a
#     manifest-command deploy cannot block on a TTY prompt in CI.
#   * The staged upload clones the active version.
#
# Reads (env):
#   FAKE_CALL_LOG                         required  the fake fastly/curl call log
#   EDGEZERO__TEST__STAGED_VERSION        required  the version the staged deploy produced

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

assert_update_flags() {
  local update="$1" flag
  for flag in --autoclone --version=active --non-interactive --service-id; do
    [[ "$update" == *"$flag"* ]] ||
      fail "'compute update' is missing $flag (got: $update)"
  done
}

assert_no_comment_on_update() {
  local log="$1"
  if grep -qE '^fastly compute update .*--comment' "$log"; then
    fail "--comment was forwarded to 'compute update', which does not support it"
  fi
}

assert_comment_precedes_stage() {
  local log="$1" comment_line stage_line
  comment_line=$(grep -nE '^fastly service-version update .*--comment' "$log" | head -n 1 | cut -d: -f1)
  stage_line=$(grep -nE '^fastly service-version stage ' "$log" | head -n 1 | cut -d: -f1)

  [[ -n "$comment_line" ]] || fail "the comment was never applied via 'service-version update'"
  [[ -n "$stage_line" ]] || fail "the version was never staged"
  [[ "$comment_line" -lt "$stage_line" ]] ||
    fail "the comment was applied after staging; it must precede it"
}

# The staging twin must MIRROR production's runtime overrides: the non-config
# override (LOG_LEVEL) is copied verbatim and the config selector is redirected
# to `<logical>_staging`, both written into the twin (STAGESEL1) before the
# relink. Without the mirror the staged version would lose production's adapter /
# logging overrides.
assert_twin_mirrors_production() {
  local log="$1"
  grep -qE '^fastly config-store-entry update .*--store-id=STAGESEL1 .*--key=EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL' "$log" ||
    fail "production's non-config override was not mirrored into the staging twin"
  grep -qE '^fastly config-store-entry update .*--store-id=STAGESEL1 .*--key=EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY' "$log" ||
    fail "the config selector was not written into the staging twin"

  # The mirror must land before the relink points the draft at the twin.
  local mirror_line create_line
  mirror_line=$(grep -nE '^fastly config-store-entry update .*--store-id=STAGESEL1' "$log" | head -n 1 | cut -d: -f1)
  create_line=$(grep -n '^fastly resource-link create ' "$log" | head -n 1 | cut -d: -f1)
  if [[ -n "$mirror_line" && -n "$create_line" ]] && ((mirror_line >= create_line)); then
    fail "the twin must be mirrored BEFORE the draft is relinked to it"
  fi
}

# The staged draft must be re-pointed at the STAGING selector store, or it reads
# production config and `config push --staging` writes a key nothing reads. The
# link name stays `edgezero_runtime_env` (what the runtime opens); only the store
# behind it changes.
assert_relinked_to_staging_selector() {
  local log="$1"
  grep -qE '^fastly resource-link delete .*--id=LINK_ENV( |$)' "$log" ||
    fail "the staged deploy never dropped the inherited 'edgezero_runtime_env' link"
  grep -qE '^fastly resource-link create .*--resource-id=STAGESEL1 .*--name=edgezero_runtime_env( |$)' "$log" ||
    fail "the staged deploy never linked the staging selector store as 'edgezero_runtime_env'"

  # Both must land while the version is still an editable draft.
  local create_line stage_line
  create_line=$(grep -n '^fastly resource-link create ' "$log" | head -n 1 | cut -d: -f1)
  stage_line=$(grep -n '^fastly service-version stage ' "$log" | head -n 1 | cut -d: -f1)
  if [[ -n "$create_line" && -n "$stage_line" ]] && ((create_line >= stage_line)); then
    fail "the staging relink must happen BEFORE the version is staged"
  fi
}

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local staged_version="${EDGEZERO__TEST__STAGED_VERSION:-}"

  echo "--- recorded fastly/curl calls:"
  cat "$log"

  local update
  update=$(grep -E '^fastly compute update ' "$log" | head -n 1 || true)
  [[ -n "$update" ]] || fail "the staged deploy never ran 'fastly compute update'"

  assert_update_flags "$update"
  assert_no_comment_on_update "$log"
  assert_comment_precedes_stage "$log"
  assert_twin_mirrors_production "$log"
  assert_relinked_to_staging_selector "$log"

  # The staged version must thread out of deploy-fastly, or the healthcheck and
  # rollback that follow have nothing to act on.
  [[ "$staged_version" == "42" ]] ||
    fail "expected fastly-version=42 out of the staged deploy, got '${staged_version:-<empty>}'"

  notice "staged call sequence is correct and fastly-version=$staged_version threaded out"
}

main "$@"
