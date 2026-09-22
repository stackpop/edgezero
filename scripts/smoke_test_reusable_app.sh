#!/usr/bin/env bash
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec cargo run --quiet --locked --manifest-path "$REPO_DIR/tests/fixtures/reusable-app/Cargo.toml" -p fixture-harness -- "$@"
