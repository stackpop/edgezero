#!/usr/bin/env bash
set -euo pipefail

# Asserts the config-push-fastly chain end to end against the fake Fastly CLI.
#
# The contract that matters is the staging model: staging and production write
# DIFFERENT KEYS in the SAME store, so a staged push can never overwrite the key
# the live service is reading.
#
# Reads (env):
#   FAKE_CALL_LOG                  required  the fake fastly/curl call log
#   EDGEZERO__TEST__STAGED_KEY     required  pushed-key output of the staging push
#   EDGEZERO__TEST__STAGED_STORE   required  store output of the staging push
#   EDGEZERO__TEST__PROD_KEY       required  pushed-key output of the production push

log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"
staged_key="${EDGEZERO__TEST__STAGED_KEY:-}"
staged_store="${EDGEZERO__TEST__STAGED_STORE:-}"
prod_key="${EDGEZERO__TEST__PROD_KEY:-}"

echo "--- recorded fastly calls:"
cat "$log" || true

fail() {
  echo "::error::$1"
  exit 1
}

# The staged push must write the _staging variant; production the base key.
[[ "$staged_key" == "app_config_staging" ]] ||
  fail "staging pushed-key should be 'app_config_staging', got '$staged_key'"
[[ "$prod_key" == "app_config" ]] ||
  fail "production pushed-key should be 'app_config', got '$prod_key'"
[[ "$staged_key" != "$prod_key" ]] ||
  fail "staging and production wrote the SAME key — staged config would overwrite live config"

# The store output must be the id the CLI RESOLVED, not the empty raw input:
# neither push passed --store.
[[ "$staged_store" == "app_config" ]] ||
  fail "store output should be the resolved logical id 'app_config', got '$staged_store'"

# The push must have gone through the real Fastly config-store surface.
grep -q 'fastly config-store list' "$log" ||
  fail "config push never resolved the store via 'fastly config-store list'"
grep -q 'fastly config-store-entry update' "$log" ||
  fail "config push never wrote an entry via 'fastly config-store-entry update'"

echo "config-push contracts OK (staged=$staged_key, prod=$prod_key, store=$staged_store)"
