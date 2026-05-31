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

for target in wasm32-wasip1 wasm32-wasip2; do
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

section "Workspace Feature Compilation"
run cargo check --workspace --all-targets --features "fastly cloudflare spin"

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

# `examples/app-demo` is excluded from the root workspace
# (per `exclude = ["examples/app-demo"]`), so the workspace
# test above doesn't cover it. Stage 8.6 wired this gate into
# CI; this script mirrors it for local runs.
section "app-demo Workspace Tests"
(
  cd examples/app-demo
  run cargo test --workspace --all-targets
)

echo "All tests completed successfully."
