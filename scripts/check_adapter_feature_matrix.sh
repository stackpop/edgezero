#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <adapter> <target|native>" >&2
  exit 2
fi

adapter="$1"
target="$2"

case "${adapter}:${target}" in
  axum:native | cloudflare:native | fastly:native | spin:native) ;;
  cloudflare:wasm32-unknown-unknown | fastly:wasm32-wasip1 | spin:wasm32-wasip2) ;;
  *)
    echo "unsupported adapter/target pair: ${adapter}:${target}" >&2
    exit 2
    ;;
esac

feature_sets=(
  ""
  "${adapter}"
  "cli"
  "test-utils"
  "${adapter},cli"
  "${adapter},test-utils"
  "cli,test-utils"
  "${adapter},cli,test-utils"
)

for features in "${feature_sets[@]}"; do
  args=(
    cargo check
    --offline
    --locked
    --package "edgezero-adapter-${adapter}"
    --no-default-features
    --all-targets
  )
  if [[ "${target}" != "native" ]]; then
    args+=(--target "${target}")
  fi
  label="none"
  if [[ -n "${features}" ]]; then
    args+=(--features "${features}")
    label="${features}"
  fi

  echo "==> edgezero-adapter-${adapter} ${target} features=[${label}]"
  "${args[@]}"
done
