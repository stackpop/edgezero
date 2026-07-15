#!/usr/bin/env bash
set -euo pipefail

# Asserts the exact Fastly call sequence a STAGED deploy through the
# deploy-fastly wrapper must produce, and that the staged version threaded out
# of the action.
#
# Regression test for real defects a review found:
#   * `--comment` was forwarded to `fastly compute update`, which has no such
#     flag, so the upload failed. It must be applied via
#     `fastly service-version update --comment` BEFORE the version is staged.
#   * A manifest-command deploy never received `--non-interactive` and could
#     block on a TTY prompt in CI. The wrapper now supplies it as an
#     action-owned passthrough arg.
#   * The staged upload must clone the active version.
#
# Reads (env): FAKE_CALL_LOG, EDGEZERO__TEST__STAGED_VERSION.

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

  # The staged version must thread out of deploy-fastly, or the healthcheck and
  # rollback that follow have nothing to act on.
  [[ "$staged_version" == "42" ]] ||
    fail "expected fastly-version=42 out of the staged deploy, got '${staged_version:-<empty>}'"

  notice "staged call sequence is correct and fastly-version=$staged_version threaded out"
}

main "$@"
