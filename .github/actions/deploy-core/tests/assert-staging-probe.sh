#!/usr/bin/env bash
set -euo pipefail

# Asserts the staging health check resolved the staged version's IP and probed
# through it.
#
# Regression test: the Fastly domain API returns a SINGULAR `staging_ip` string
# per domain object (`staging_ips` is only the `include=` query param). Reading
# it as an array silently found no IP and probed production instead.
#
# Inputs (environment): FAKE_CALL_LOG.

main() {
  local log="${FAKE_CALL_LOG:?FAKE_CALL_LOG is required}"

  if ! grep -qE '^GET https://api\.fastly\.com/service/dummy-service/version/42/domain\?include=staging_ips$' "$log"; then
    echo "::error::the staging-IP lookup was never performed" >&2
    return 1
  fi

  if ! grep -qE '^PROBE .*--connect-to ::151\.101\.2\.10:443 .*https://staging\.example\.com/' "$log"; then
    echo "::error::the probe was not rerouted to the staging IP (singular staging_ip not read?)" >&2
    return 1
  fi

  echo "staging probe was rerouted through the resolved staging IP"
}

main "$@"
