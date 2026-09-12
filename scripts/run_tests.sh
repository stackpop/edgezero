#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

command -v cargo >/dev/null 2>&1 || {
  echo "cargo is required but was not found in PATH" >&2
  exit 1
}

command -v rustup >/dev/null 2>&1 || {
  echo "rustup is required to verify the wasm32-wasip1 / wasm32-wasip2 targets" >&2
  exit 1
}

for target in wasm32-unknown-unknown wasm32-wasip1 wasm32-wasip2; do
  if ! rustup target list --installed | grep -Fxq "$target"; then
    echo "$target target is not installed. Run 'rustup target add $target' before re-running this script." >&2
    exit 1
  fi
done

run() {
  echo "==> $*"
  "$@"
}

section() {
  printf '\n%s\n' "=============================="
  printf '%s\n' "${1}"
  printf '%s\n' "=============================="
}

section "Workspace Tests"
run cargo test --workspace --all-targets

section "Outbound Contract Tests"
run scripts/run_test_nonzero.sh send_all_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-axum --no-default-features --features axum,test-utils --test contract
run scripts/run_test_nonzero.sh send_all_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-cloudflare --no-default-features --features test-utils --test contract
run scripts/run_test_nonzero.sh send_all_dispatches_every_slot_before_wait cargo test --offline --locked -p edgezero-adapter-fastly --no-default-features --features test-utils --test contract
run scripts/run_test_nonzero.sh send_all_preflight_precedence_and_indices cargo test --offline --locked -p edgezero-adapter-spin --no-default-features --features test-utils --test contract

section "Outbound Capability Tests"
for adapter in axum cloudflare fastly spin; do
  run scripts/run_test_nonzero.sh adapter_capability_matrix_matches_contracts cargo test --offline --locked -p "edgezero-adapter-${adapter}" --no-default-features --features cli --lib adapter_capability_matrix_matches_contracts
done

section "Workspace Feature Compilation"
run cargo check --workspace --all-targets --features "fastly cloudflare spin"

section "Adapter Native Feature Matrices"
for adapter in axum cloudflare fastly spin; do
  run bash scripts/check_adapter_feature_matrix.sh "${adapter}" native
done

section "Adapter Wasm Feature Matrices"
run bash scripts/check_adapter_feature_matrix.sh cloudflare wasm32-unknown-unknown
run bash scripts/check_adapter_feature_matrix.sh fastly wasm32-wasip1
run bash scripts/check_adapter_feature_matrix.sh spin wasm32-wasip2

section "Fastly CLI Tests"
run cargo test -p edgezero-adapter-fastly --no-default-features --features cli

section "Fastly Wasm Tests"
(
  cd crates/edgezero-adapter-fastly
  run cargo test --features fastly --target wasm32-wasip1 -- --nocapture
)

# Spin 6.0 compiles to wasm32-wasip2; CI runs the full contract
# test under wasmtime. Locally we just check it compiles — the
# contract test needs wasmtime + the wasm runner pinned in CI.
section "Spin Wasm Compile Check"
run cargo check -p edgezero-adapter-spin --features spin --target wasm32-wasip2

section "Generated Project"
run scripts/run_test_nonzero.sh --ignored generated_workspace_compiles cargo test --offline --locked -p edgezero-cli --test generated_project_builds

# `examples/app-demo` is excluded from the root workspace
# (per `exclude = ["examples/app-demo"]`), so the workspace
# test above doesn't cover it. Stage 8.6 wired this gate into
# CI; this script mirrors it for local runs.
section "app-demo Workspace Tests"
(
  cd examples/app-demo
  run cargo test --locked --workspace --all-targets
  run cargo check --locked -p app-demo-adapter-cloudflare --target wasm32-unknown-unknown --no-default-features --features cloudflare
  run cargo check --locked -p app-demo-adapter-fastly --target wasm32-wasip1 --no-default-features --features fastly
  run cargo check --locked -p app-demo-adapter-spin --target wasm32-wasip2 --no-default-features --features spin
)

section "Outbound Documentation Contracts"
run bash scripts/check_outbound_legacy_api.sh
run node scripts/check_outbound_docs_contract.mjs

echo "All tests completed successfully."
