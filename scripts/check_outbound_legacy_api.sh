#!/usr/bin/env bash
set -uo pipefail

scan() {
  local pattern=$1
  shift

  if matches="$(git grep -nE "$pattern" -- "$@")"; then
    printf '%s\n' "$matches"
    return 1
  fi

  local status=$?
  if [ "$status" -eq 1 ]; then
    return 0
  fi
  return "$status"
}

failed=0

scan \
  'Proxy(Client|Handle|Request|Response|Service)|proxy_handle|edgezero_core::proxy|crate::proxy|pub mod proxy' \
  crates examples/app-demo docs/guide README.md CLAUDE.md TODO.md Cargo.toml \
  .claude/agents/code-architect.md || failed=1

# Downstream response delivery is now adapter-owned. Keep the outbound-fetch
# buffering helpers: they implement ResponseMode::Buffered and are unrelated to
# response egress.
scan \
  'ResponseReturned|begin_owned|[A-Z]+_RESPONSE_STREAM_BUFFER_BYTES|SpinFullResponse|Captured(Cloudflare|Spin)Response|EdgeZeroAxumService|dispatch_for_test|pub (async )?fn (from_core_response|into_axum_response)|buffered (fallback|passthrough)|16 MiB downstream conversion fallback|Response -> Workers Response \(buffered bodies\)' \
  crates examples/app-demo docs/guide README.md TODO.md Cargo.toml \
  ':(exclude)crates/edgezero-cli/src/generator.rs' || failed=1

# The generator keeps a negative assertion containing this spelling. Scan the
# rendered source inputs, demos, and user-facing guides where an attribute would
# preserve the retired Fastly response-returning entrypoint.
scan \
  '#\[fastly::main\]' \
  crates/edgezero-adapter-fastly/src/templates examples/app-demo docs/guide README.md || failed=1

exit "$failed"
