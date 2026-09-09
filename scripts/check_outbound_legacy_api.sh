#!/usr/bin/env bash
set -uo pipefail

if matches="$(rg -n 'Proxy(Client|Handle|Request|Response|Service)|proxy_handle|edgezero_core::proxy|crate::proxy|pub mod proxy' \
  crates examples/app-demo --glob '*.rs' --glob '*.hbs')"; then
  printf '%s\n' "$matches"
  exit 1
else
  status=$?
  if [ "$status" -eq 1 ]; then
    exit 0
  fi
  exit "$status"
fi
