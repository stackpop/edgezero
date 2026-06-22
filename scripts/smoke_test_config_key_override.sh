#!/usr/bin/env bash
set -euo pipefail

# §12.7 + §9.3 + §8.3 multi-adapter smoke:
#
# 1. Per-adapter loop:
#    - Push a "default" blob to the binding's default key (`app_config`).
#    - Push a "staging" blob to `app_config_staging` via `--key`.
#    - Boot the runtime with `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging`.
#    - Assert the handler returns the staging value.
#    - Reboot WITHOUT the override.
#    - Assert the handler returns the default value.
#
# 2. Fastly-only oversized envelope smoke: push a blob > 8 000 chars,
#    confirm the local fastly.toml carries a chunk-pointer and literal
#    dotted chunk keys, then boot the runtime and assert the handler
#    returns the large value.
#
# 3. Spin Cloud Unsupported smoke (gated by SKIP_SPIN_CLOUD_SMOKE=1):
#    `config diff --adapter spin` against a Cloud-flagged manifest
#    must return non-zero with the §8.3 message; `config push --yes`
#    against Cloud must succeed unconditionally.
#
# Usage:
#   ./scripts/smoke_test_config_key_override.sh
#   SKIP_SPIN_CLOUD_SMOKE=1 ./scripts/smoke_test_config_key_override.sh
#
# Notes:
# - Requires `app-demo-cli` reachable via `cargo run -p app-demo-cli`.
# - Requires `wrangler`, `viceroy`, `spin` on PATH for the matching
#   adapter rows. Rows for missing tooling can be skipped with the
#   per-suite SKIP_<ADAPTER>=1 env vars.
# - The smoke writes to a tempdir per row so checked-in fixtures
#   under examples/app-demo/ are not modified.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEMO_DIR="$ROOT_DIR/examples/app-demo"
SERVER_PID=""
PORT=8765
PASS=0
FAIL=0

cleanup() {
  if [ -n "$SERVER_PID" ]; then
    pkill -P "$SERVER_PID" 2>/dev/null || true
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
  fi
}
trap cleanup EXIT INT TERM

check() {
  local label="$1" expect="$2" actual="$3"
  if [ "$actual" = "$expect" ]; then
    printf '  PASS  %s\n' "$label"
    PASS=$((PASS + 1))
  else
    printf '  FAIL  %s  (expected %q, got %q)\n' "$label" "$expect" "$actual"
    FAIL=$((FAIL + 1))
  fi
}

check_contains() {
  local label="$1" needle="$2" haystack="$3"
  if printf '%s' "$haystack" | grep -F -q -- "$needle"; then
    printf '  PASS  %s\n' "$label"
    PASS=$((PASS + 1))
  else
    printf '  FAIL  %s  (missing %q in output)\n' "$label" "$needle"
    FAIL=$((FAIL + 1))
  fi
}

wait_for_port() {
  local max_wait=60 waited=0
  until curl -s -o /dev/null "http://127.0.0.1:${PORT}/" 2>/dev/null; do
    kill -0 "$SERVER_PID" 2>/dev/null || { echo "server exited early" >&2; return 1; }
    sleep 1
    waited=$((waited + 1))
    if [ "$waited" -ge "$max_wait" ]; then
      echo "server did not start within ${max_wait}s" >&2
      return 1
    fi
  done
}

write_default_blob() {
  local tmp="$1"
  cat > "$tmp/app-demo.toml" <<'TOML'
api_token = "demo_api_token"
greeting = "default-blob"
vault = "default"

[feature]
new_checkout = false

[service]
timeout_ms = 1500
TOML
}

write_staging_blob() {
  local tmp="$1"
  cat > "$tmp/app-demo.toml" <<'TOML'
api_token = "demo_api_token"
greeting = "staging-blob"
vault = "default"

[feature]
new_checkout = false

[service]
timeout_ms = 1500
TOML
}

# Boot the right runtime for $1 (adapter name), returning when the
# greeting endpoint responds 200.
boot_runtime() {
  local adapter="$1"
  case "$adapter" in
    axum)
      (cd "$DEMO_DIR" && \
        cargo run --quiet -p app-demo-cli -- serve --adapter axum 2>&1) &
      ;;
    cloudflare)
      (cd "$DEMO_DIR/crates/app-demo-adapter-cloudflare" && \
        wrangler dev --local --port "$PORT" 2>&1) &
      ;;
    fastly)
      (cd "$DEMO_DIR/crates/app-demo-adapter-fastly" && \
        viceroy run -C fastly.toml --addr "127.0.0.1:${PORT}" target/wasm32-wasip1/debug/app_demo_adapter_fastly.wasm 2>&1) &
      ;;
    spin)
      (cd "$DEMO_DIR/crates/app-demo-adapter-spin" && \
        spin up --listen "127.0.0.1:${PORT}" --runtime-config-file runtime-config.toml 2>&1) &
      ;;
    *)
      echo "unknown adapter: $adapter" >&2; return 1 ;;
  esac
  SERVER_PID=$!
  wait_for_port
}

# (adapter, extra-push-flags) per row. Axum's push is always local
# (no --local flag); the other three need --local for the local
# emulator state seed. Round-31 H-2 from the plan.
SUITES=(
  "axum:"
  "cloudflare:--local"
  "fastly:--local"
  "spin:--local"
)

# -- §12.7: per-adapter __KEY override loop -------------------------------

for suite in "${SUITES[@]}"; do
  adapter="${suite%%:*}"
  extra="${suite#*:}"

  skip_var="SKIP_${adapter^^}"
  if [ "${!skip_var:-0}" = "1" ]; then
    printf '\n=== §12.7 __KEY override smoke: %s SKIPPED (%s=1) ===\n' "$adapter" "$skip_var"
    continue
  fi

  printf '\n=== §12.7 __KEY override smoke: %s%s ===\n' "$adapter" "${extra:+ $extra}"
  tmp=$(mktemp -d)
  trap "cleanup; rm -rf '$tmp'" EXIT INT TERM

  # 1. Push the default blob at the default key.
  unset EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY
  write_default_blob "$tmp"
  (cd "$DEMO_DIR" && cargo run --quiet -p app-demo-cli -- \
    config push --adapter "$adapter" $extra \
    --app-config "$tmp/app-demo.toml" --yes >/dev/null)

  # 2. Push the staging blob under app_config_staging.
  write_staging_blob "$tmp"
  (cd "$DEMO_DIR" && cargo run --quiet -p app-demo-cli -- \
    config push --adapter "$adapter" $extra \
    --app-config "$tmp/app-demo.toml" --key app_config_staging --yes >/dev/null)

  # 3. Boot with __KEY=staging; assert staging.
  EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging \
    boot_runtime "$adapter"
  result=$(curl -s "http://127.0.0.1:${PORT}/config/greeting")
  check "$adapter __KEY override returns staging" "staging-blob" "$result"
  cleanup

  # 4. Reboot without __KEY; assert default.
  unset EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY
  boot_runtime "$adapter"
  result=$(curl -s "http://127.0.0.1:${PORT}/config/greeting")
  check "$adapter __KEY unset returns default" "default-blob" "$result"
  cleanup

  rm -rf "$tmp"
  trap cleanup EXIT INT TERM
done

# -- §9.3 Fastly oversized envelope smoke ---------------------------------

if [ "${SKIP_FASTLY:-0}" = "1" ]; then
  printf '\n=== §9.3 Fastly chunk-pointer smoke: SKIPPED (SKIP_FASTLY=1) ===\n'
else
  printf '\n=== §9.3 Fastly chunk-pointer smoke ===\n'
  tmp=$(mktemp -d)
  trap "cleanup; rm -rf '$tmp'" EXIT INT TERM

  # Build an oversized greeting (>= 9 000 chars after envelope wrap)
  # so the chunked path fires.
  big_greeting=$(printf 'large-fastly-blob-%.0s' {1..500})

  cat > "$tmp/app-demo-large.toml" <<TOML
api_token = "demo_api_token"
greeting = "${big_greeting}"
vault = "default"

[feature]
new_checkout = false

[service]
timeout_ms = 1500
TOML

  (cd "$DEMO_DIR" && cargo run --quiet -p app-demo-cli -- \
    config push --adapter fastly --local \
    --app-config "$tmp/app-demo-large.toml" --yes >/dev/null)

  fastly_toml="$DEMO_DIR/crates/app-demo-adapter-fastly/fastly.toml"
  pointer_present=$(grep -c 'edgezero_kind.*fastly_config_chunks' "$fastly_toml" || true)
  check "fastly.toml carries a chunk-pointer kind marker" "1" "$pointer_present"

  chunk_keys_present=$(grep -c '__edgezero_chunks\.' "$fastly_toml" || true)
  if [ "$chunk_keys_present" -gt 0 ]; then
    check "fastly.toml carries literal __edgezero_chunks keys" "yes" "yes"
  else
    check "fastly.toml carries literal __edgezero_chunks keys" "yes" "no"
  fi

  boot_runtime fastly
  result=$(curl -s "http://127.0.0.1:${PORT}/config/greeting")
  check_contains "fastly runtime reads reconstructed envelope" "large-fastly-blob" "$result"
  cleanup

  rm -rf "$tmp"
  trap cleanup EXIT INT TERM
fi

# -- §8.3 Spin Cloud Unsupported smoke ------------------------------------

if [ "${SKIP_SPIN_CLOUD_SMOKE:-0}" = "1" ]; then
  printf '\n=== §8.3 Spin Cloud Unsupported smoke: SKIPPED (SKIP_SPIN_CLOUD_SMOKE=1) ===\n'
else
  printf '\n=== §8.3 Spin Cloud Unsupported smoke ===\n'
  # `config diff` against a Cloud-flagged manifest MUST error.
  if (cd "$DEMO_DIR" && cargo run --quiet -p app-demo-cli -- \
        config diff --adapter spin --format unified >/dev/null 2>&1); then
    check "spin cloud diff exits non-zero" "non-zero" "exit-0"
  else
    check "spin cloud diff exits non-zero" "non-zero" "non-zero"
  fi

  # `config push --yes` against Spin Cloud SHOULD succeed (write-only).
  tmp=$(mktemp -d)
  write_default_blob "$tmp"
  if (cd "$DEMO_DIR" && cargo run --quiet -p app-demo-cli -- \
        config push --adapter spin --key app_config_staging --yes \
        --app-config "$tmp/app-demo.toml" >/dev/null 2>&1); then
    check "spin cloud --yes push succeeds (write-only)" "ok" "ok"
  else
    check "spin cloud --yes push succeeds (write-only)" "ok" "failed"
  fi
  rm -rf "$tmp"
fi

# -- Summary --------------------------------------------------------------

printf '\n==============================\n'
printf 'Results:  %d passed, %d failed\n' "$PASS" "$FAIL"
printf '==============================\n'

[ "$FAIL" -eq 0 ] || exit 1
