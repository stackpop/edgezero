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

# The completion-driven hard cut removes the terminal-vector-only API and the
# request-extension-only Fastly lifecycle. The generator's negative assertion
# intentionally contains `.send_all(` and is excluded here.
scan \
  '\.send_all\(|SendAllSlotIsolation|send-all-slot-isolation|run_app_with_request_extensions' \
  crates examples/app-demo docs/guide README.md CLAUDE.md TODO.md \
  ':(exclude)crates/edgezero-cli/src/generator.rs' || failed=1

# Batch termination is explicit. Adapter drivers cannot use stream EOF as a
# cutoff signal, and callers cannot interpret every missing item as timeout.
scan \
  'OutboundBatch::from_stream|while[[:space:]]+let[[:space:]]+Some\([^)]*\)[[:space:]]*=[[:space:]]*batch\.next\(\)\.await|send_all_until\(.*\)\.await\.slots|Result<OutboundBatchResults,[[:space:]]*EdgeError>' \
  crates examples/app-demo docs/guide README.md CLAUDE.md TODO.md || failed=1

# Generated/demo Fastly entrypoints must delegate transmission to EdgeZero.
scan \
  'stream_to_client|send_to_client|send_with_registries' \
  crates/edgezero-adapter-fastly/src/templates examples/app-demo/crates/app-demo-adapter-fastly || failed=1

# Application assembly and ingress response ownership are hard-cut contracts.
# Configuration must be fallible, every response-producing admission variant
# uses named fields, and response completion never travels through HTTP
# extensions or an envelope escape hatch.
scan \
  'fn configure(_app)?\([^)]*&mut (EdgeZeroApp|edgezero_core::app::App|App)[^)]*\)[[:space:]]*\{' \
  crates examples/app-demo docs/guide README.md || failed=1

scan \
  'fn build_app\(\)[[:space:]]*->[[:space:]]*App|AdmissionDecision::Refuse[[:space:]]*\(|ResponseEgressEnvelope[^[:space:]]*\.into_response|envelope\.into_response\(\)' \
  crates examples/app-demo docs/guide README.md || failed=1

scan \
  '(extensions(_mut)?\(\)|extensions)\.insert\(ResponseEgressCompletion' \
  crates examples/app-demo docs/guide README.md \
  ':(exclude)crates/edgezero-core/src/response_egress.rs' || failed=1

# Normalized ingress errors use one typed send-or-abort decision. The generator
# keeps the retired setter spelling only in a negative assertion.
scan \
  'DetachedResponseEgressCompletionFactory|set_detached_response_egress_completion_factory' \
  crates examples/app-demo docs/guide docs/superpowers/specs README.md \
  ':(exclude)crates/edgezero-cli/src/generator.rs' || failed=1

exit "$failed"
