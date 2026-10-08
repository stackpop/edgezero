#!/usr/bin/env bash
# Independent normal dependency graphs; workspace --all-features is not isolation proof.
set -euo pipefail

assert_no_sdk() {
  local graph
  graph="$(cargo tree --offline --locked --edges normal --prefix none "$@")"
  if grep -Eq '^aws-(config|sdk-[^ ]+) v' <<< "$graph"; then
    printf 'AWS SDK leaked into isolated graph: %s\n' "$*" >&2
    exit 1
  fi
}

assert_no_sdk -p edgezero-cli --no-default-features --features cli,edgezero-adapter-axum
assert_no_sdk -p edgezero-store-aws --no-default-features
assert_no_sdk -p edgezero-store-aws --no-default-features --features appconfig-agent
assert_no_sdk -p edgezero-adapter-axum --no-default-features --features axum
assert_no_sdk -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare
assert_no_sdk -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly
assert_no_sdk -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
printf 'Native store SDK isolation graphs passed.\n'
