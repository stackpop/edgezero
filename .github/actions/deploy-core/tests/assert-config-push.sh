#!/usr/bin/env bash
set -euo pipefail

# Asserts one config-push-fastly invocation against the fake Fastly CLI.
#
# The contract that matters is the staging model: staging and production write
# DIFFERENT KEYS in the SAME store, so a staged push can never overwrite the key
# the live service is reading. This runs once per push — re-seeding the fake
# truncates the call log, so each push is asserted against its own log.
#
# Reads (env):
#   FAKE_CALL_LOG                 required  the fake fastly call log
#   EDGEZERO__TEST__EXPECT_KEY    required  the key this push must have written
#   EDGEZERO__TEST__PUSHED_KEY    required  the action's pushed-key output
#   EDGEZERO__TEST__PUSHED_STORE  required  the action's store output
#   EDGEZERO__TEST__REJECT_KEY    optional  a key that must NOT appear in the log

log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
expect_key="${EDGEZERO__TEST__EXPECT_KEY:?EDGEZERO__TEST__EXPECT_KEY is required}"
pushed_key="${EDGEZERO__TEST__PUSHED_KEY:-}"
pushed_store="${EDGEZERO__TEST__PUSHED_STORE:-}"
reject_key="${EDGEZERO__TEST__REJECT_KEY:-}"

echo "--- recorded fastly calls:"
cat "$log" || true

fail() {
  echo "::error::$1"
  exit 1
}

# The action's own output must name the key it wrote.
[[ "$pushed_key" == "$expect_key" ]] ||
  fail "pushed-key output should be '$expect_key', got '$pushed_key'"

# The store output must be the id the CLI RESOLVED from the manifest — neither
# push passes --store, so an empty value would mean the wrapper echoed its input.
[[ "$pushed_store" == "app_config" ]] ||
  fail "store output should be the resolved logical id 'app_config', got '$pushed_store'"

# The push must have gone through the real Fastly config-store surface, and
# written the expected key.
grep -q 'fastly config-store list' "$log" ||
  fail "config push never resolved the store via 'fastly config-store list'"
grep -qE "fastly config-store-entry update .*--key=${expect_key}( |$)" "$log" ||
  fail "config push never wrote --key=$expect_key via 'fastly config-store-entry update'"

# Staging must not touch the production key (and vice versa).
if [[ -n "$reject_key" ]]; then
  if grep -qE "config-store-entry update .*--key=${reject_key}( |$)" "$log"; then
    fail "this push wrote --key=$reject_key, which it must never touch"
  fi
fi

echo "config-push OK (wrote $expect_key in store $pushed_store)"
