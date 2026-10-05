#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
host_triple="$(rustc -vV | awk '$1 == "host:" { print $2 }')"
test -n "$host_triple"

# Job control gives the bootstrap build its own process group on Linux and macOS.
set -m
bootstrap_pid=""
cleanup_bootstrap() {
  if [[ -n "$bootstrap_pid" ]]; then
    kill -TERM -- "-$bootstrap_pid" 2>/dev/null || true
    kill -KILL -- "-$bootstrap_pid" 2>/dev/null || true
    wait "$bootstrap_pid" 2>/dev/null || true
  fi
}
trap cleanup_bootstrap EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
cargo build --manifest-path "$repo_root/tests/fixtures/request-fidelity-runner/Cargo.toml" \
  --locked --target "$host_triple" --target-dir "$repo_root/target/request-fidelity-runner" --quiet &
bootstrap_pid=$!
wait "$bootstrap_pid"
cleanup_bootstrap
bootstrap_pid=""
trap - EXIT INT TERM HUP
set +m
exec "$repo_root/target/request-fidelity-runner/$host_triple/debug/request-fidelity-runner" \
  "$@" --repo-root "$repo_root"
