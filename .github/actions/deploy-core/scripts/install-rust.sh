#!/usr/bin/env bash
set -euo pipefail

# Installs the application Rust toolchain plus the wrapper-provided concrete
# target. The engine never maps adapter -> target; the wrapper supplies it.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

RUST_TOOLCHAIN=${RUST_TOOLCHAIN:?RUST_TOOLCHAIN is required}
TARGET=${RUST_TARGET:?RUST_TARGET is required (wrapper-provided concrete target)}
require_cmd rustup
rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal
rustup target add "$TARGET" --toolchain "$RUST_TOOLCHAIN"
append_env RUSTUP_TOOLCHAIN "$RUST_TOOLCHAIN"
