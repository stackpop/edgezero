#!/usr/bin/env bash
set -euo pipefail

# Asserts the exact Fastly call sequence a staged deploy must produce.
#
# Regression test for two real defects a review found:
#   * `--comment` was forwarded to `fastly compute update`, which has no such
#     flag — the upload failed. It must instead be applied via
#     `fastly service-version update --comment`, BEFORE the version is staged.
#   * The staged upload must clone the active version and stay non-interactive.
#
# Inputs (environment): FAKE_CALL_LOG.

require_call() {
  local pattern="$1" what="$2" log="$3"
  if ! grep -qE -- "$pattern" "$log"; then
    echo "::error::$what" >&2
    return 1
  fi
}

assert_update_flags() {
  local update="$1" flag
  for flag in --autoclone --version=active --non-interactive --service-id; do
    if [[ "$update" != *"$flag"* ]]; then
      echo "::error::'compute update' is missing $flag (got: $update)" >&2
      return 1
    fi
  done
}

assert_no_comment_on_update() {
  local log="$1"
  if grep -qE '^fastly compute update .*--comment' "$log"; then
    echo "::error::--comment was forwarded to 'compute update', which does not support it" >&2
    return 1
  fi
}

assert_comment_precedes_stage() {
  local log="$1" comment_line stage_line
  comment_line=$(grep -nE '^fastly service-version update .*--comment' "$log" | head -n 1 | cut -d: -f1)
  stage_line=$(grep -nE '^fastly service-version stage ' "$log" | head -n 1 | cut -d: -f1)

  if [[ -z "$comment_line" ]]; then
    echo "::error::the comment was never applied via 'service-version update'" >&2
    return 1
  fi
  if [[ -z "$stage_line" ]]; then
    echo "::error::the version was never staged" >&2
    return 1
  fi
  if [[ "$comment_line" -ge "$stage_line" ]]; then
    echo "::error::the comment was applied after staging; it must precede it" >&2
    return 1
  fi
}

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
  local update

  echo "--- recorded fastly/curl calls:"
  cat "$log"

  update=$(grep -E '^fastly compute update ' "$log" | head -n 1 || true)
  if [[ -z "$update" ]]; then
    echo "::error::the staged deploy never ran 'fastly compute update'" >&2
    return 1
  fi

  assert_update_flags "$update"
  assert_no_comment_on_update "$log"
  assert_comment_precedes_stage "$log"

  echo "staged call sequence is correct"
}

main "$@"
