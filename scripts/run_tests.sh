#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

command -v cargo >/dev/null 2>&1 || {
  echo "cargo is required but was not found in PATH" >&2
  exit 1
}

command -v rustup >/dev/null 2>&1 || {
  echo "rustup is required to verify the wasm32-wasip1 target" >&2
  exit 1
}

if ! rustup target list --installed | grep -Fxq 'wasm32-wasip1'; then
  echo "wasm32-wasip1 target is not installed. Run 'rustup target add wasm32-wasip1' before re-running this script." >&2
  exit 1
fi

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
run cargo test --workspace

section "Fastly CLI Tests"
run cargo test -p edgezero-adapter-fastly --no-default-features --features cli

section "Fastly Wasm Tests"
(
  cd crates/edgezero-adapter-fastly
  run cargo test --features fastly --target wasm32-wasip1 -- --nocapture
)

echo "All tests completed successfully."
