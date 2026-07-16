#!/usr/bin/env bash
set -euo pipefail

# Smoke-test the secret-store demo handlers by starting an adapter, running
# checks, and tearing it down automatically.
#
# Usage:
#   ./scripts/smoke_test_secrets.sh              # defaults to axum
#   ./scripts/smoke_test_secrets.sh axum
#   ./scripts/smoke_test_secrets.sh fastly
#   ./scripts/smoke_test_secrets.sh cloudflare
#   ./scripts/smoke_test_secrets.sh spin
#
# Note (spin): Spin variable names are lowercase.  SpinSecretStore normalises
# the key to lowercase before lookup, so "SMOKE_SECRET" maps to the Spin
# variable "smoke_secret".  The secret value is passed at startup via
# SPIN_VARIABLE_SMOKE_SECRET.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEMO_DIR="$ROOT_DIR/examples/app-demo"
ADAPTER="${1:-axum}"
SERVER_PID=""
DEV_VARS_FILE=""
SMOKE_SECRET_NAME="SMOKE_SECRET"
MISSING_SECRET_NAME="SMOKE_SECRET_MISSING"

# Warm up per-adapter local state — provision --local synthesises
# wrangler.toml / fastly.toml / spin.toml / runtime-config.toml
# and writes .dev.vars / .env / .edgezero/.env. Fresh clones need
# this because Task 33 gitignored those files. Crucial for this
# smoke: the typed dispatch (Task 30b) writes SPIN_VARIABLE_* /
# .dev.vars placeholders that the emulator boot reads.
# shellcheck source=lib/smoke_warmup.sh
. "$ROOT_DIR/scripts/lib/smoke_warmup.sh"
echo "==> Warming up local state (provision --adapter $ADAPTER --local)..."
smoke_warmup_provision_local "$ADAPTER"
DISALLOWED_SECRET_NAME="API_KEY"
SMOKE_SECRET_VALUE="smoke-secret-$(date +%s)-$$"
PASS=0
FAIL=0

export SMOKE_SECRET="$SMOKE_SECRET_VALUE"

cleanup() {
  if [ -n "$SERVER_PID" ]; then
    echo ""
    echo "==> Stopping server (PID $SERVER_PID)..."
    pkill -P "$SERVER_PID" 2>/dev/null || true
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi

  if [ -n "$DEV_VARS_FILE" ] && [ -f "$DEV_VARS_FILE" ]; then
    rm -f "$DEV_VARS_FILE"
  fi
}
trap cleanup EXIT

section() {
  printf '\n--- %s ---\n' "$1"
}

check() {
  local label="$1" expect="$2" actual="$3"
  if [ "$actual" = "$expect" ]; then
    printf '  PASS  %s\n' "$label"
    PASS=$((PASS + 1))
  else
    printf '  FAIL  %s  (expected %s, got %s)\n' "$label" "$expect" "$actual"
    FAIL=$((FAIL + 1))
  fi
}

check_contains() {
  local label="$1" needle="$2" haystack="$3"
  if [[ "$haystack" == *"$needle"* ]]; then
    printf '  PASS  %s\n' "$label"
    PASS=$((PASS + 1))
  else
    printf '  FAIL  %s  (expected body to contain %s)\n' "$label" "$needle"
    FAIL=$((FAIL + 1))
  fi
}

check_not_contains() {
  local label="$1" needle="$2" haystack="$3"
  if [[ "$haystack" == *"$needle"* ]]; then
    printf '  FAIL  %s  (body unexpectedly contained %s)\n' "$label" "$needle"
    FAIL=$((FAIL + 1))
  else
    printf '  PASS  %s\n' "$label"
    PASS=$((PASS + 1))
  fi
}

start_server() {
  case "$ADAPTER" in
    axum)
      PORT=8787
      echo "==> Building app-demo (axum)..."
      (cd "$DEMO_DIR" && cargo build -p app-demo-adapter-axum 2>&1)
      echo "==> Starting Axum adapter on port $PORT..."
      (cd "$DEMO_DIR" && cargo run -p app-demo-adapter-axum 2>&1) &
      SERVER_PID=$!
      ;;
    fastly)
      PORT=7676
      command -v fastly >/dev/null 2>&1 || {
        echo "Fastly CLI is required. Install from https://developer.fastly.com/reference/cli/" >&2
        exit 1
      }
      echo "==> Starting Fastly Viceroy on port $PORT..."
      (cd "$DEMO_DIR" && fastly compute serve -C crates/app-demo-adapter-fastly 2>&1) &
      SERVER_PID=$!
      ;;
    cloudflare)
      PORT=8787
      command -v wrangler >/dev/null 2>&1 || {
        echo "wrangler is required. Install with 'npm i -g wrangler'" >&2
        exit 1
      }
      DEV_VARS_FILE="$DEMO_DIR/crates/app-demo-adapter-cloudflare/.dev.vars"
      printf '%s=%s\n' "$SMOKE_SECRET_NAME" "$SMOKE_SECRET_VALUE" > "$DEV_VARS_FILE"
      echo "==> Starting Cloudflare wrangler dev on port $PORT..."
      (cd "$DEMO_DIR" && wrangler dev --cwd crates/app-demo-adapter-cloudflare --port "$PORT" 2>&1) &
      SERVER_PID=$!
      ;;
    spin)
      PORT=3000
      command -v spin >/dev/null 2>&1 || {
        echo "Spin CLI is required. Install from https://developer.fermyon.com/spin/v3/install" >&2
        exit 1
      }
      echo "==> Building Spin WASM (wasm32-wasip2)..."
      (cd "$DEMO_DIR" && cargo build --target wasm32-wasip2 --release -p app-demo-adapter-spin 2>&1)
      echo "==> Starting Spin on port $PORT..."
      # SpinSecretStore normalises the key to lowercase, so SMOKE_SECRET maps to
      # the Spin variable smoke_secret.  Pass the value via SPIN_VARIABLE_SMOKE_SECRET.
      # `--runtime-config-file runtime-config.toml`: the demo's
      # spin.toml declares non-`default` KV labels (`app_config`,
      # `sessions`, `cache`) — Spin's runtime needs the file or
      # `spin up` aborts with `unknown key_value_stores label
      # <name>` before secrets are exercised.
      (cd "$DEMO_DIR/crates/app-demo-adapter-spin" && \
        SPIN_VARIABLE_SMOKE_SECRET="$SMOKE_SECRET_VALUE" \
        spin up --listen "127.0.0.1:$PORT" \
          --runtime-config-file runtime-config.toml 2>&1) &
      SERVER_PID=$!
      ;;
    *)
      echo "Unknown adapter: $ADAPTER" >&2
      echo "Usage: $0 [axum|fastly|cloudflare|spin]" >&2
      exit 1
      ;;
  esac
}

wait_for_server() {
  BASE="http://127.0.0.1:${PORT}"

  echo "==> Waiting for server at $BASE ..."
  MAX_WAIT=60
  WAITED=0
  until curl -fsS -o /dev/null "$BASE/" 2>/dev/null; do
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
      echo "Server process exited early" >&2
      return 1
    fi
    sleep 1
    WAITED=$((WAITED + 1))
    if [ "$WAITED" -ge "$MAX_WAIT" ]; then
      echo "Server did not start within ${MAX_WAIT}s" >&2
      return 1
    fi
  done
  echo "==> Server ready (${WAITED}s)"
}

run_checks() {
  section "Health check"
  STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/")
  check "GET / returns 200" "200" "$STATUS"

  section "Secret echo"
  STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/secrets/echo?name=$SMOKE_SECRET_NAME")
  check "GET /secrets/echo?name=$SMOKE_SECRET_NAME returns 200" "200" "$STATUS"

  BODY=$(curl -s "$BASE/secrets/echo?name=$SMOKE_SECRET_NAME")
  check "GET /secrets/echo?name=$SMOKE_SECRET_NAME returns secret value" "$SMOKE_SECRET_VALUE" "$BODY"

  STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/secrets/echo?name=$MISSING_SECRET_NAME")
  check "GET /secrets/echo?name=$MISSING_SECRET_NAME returns 500" "500" "$STATUS"

  BODY=$(curl -s "$BASE/secrets/echo?name=$MISSING_SECRET_NAME")
  check_contains \
    "Missing allowed secret response is sanitized" \
    "required secret is not configured" \
    "$BODY"
  check_not_contains \
    "Missing allowed secret response does not leak the key name" \
    "$MISSING_SECRET_NAME" \
    "$BODY"

  STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/secrets/echo?name=$DISALLOWED_SECRET_NAME")
  check "GET /secrets/echo?name=$DISALLOWED_SECRET_NAME returns 400" "400" "$STATUS"

  BODY=$(curl -s "$BASE/secrets/echo?name=$DISALLOWED_SECRET_NAME")
  check_contains \
    "Disallowed secret name returns a policy error" \
    "only smoke-test secret names are allowed" \
    "$BODY"
  check_not_contains \
    "Disallowed secret name response does not echo user input" \
    "$DISALLOWED_SECRET_NAME" \
    "$BODY"
}

start_server

if wait_for_server; then
  run_checks
else
  FAIL=$((FAIL + 1))
  echo "==> Skipping checks because the server did not become ready"
fi

printf '\n==============================\n'
printf 'Adapter:  %s\n' "$ADAPTER"
printf 'Secret:   %s\n' "$SMOKE_SECRET_NAME"
printf 'Missing:  %s\n' "$MISSING_SECRET_NAME"
printf 'Results:  %d passed, %d failed\n' "$PASS" "$FAIL"
printf '==============================\n'

[ "$FAIL" -eq 0 ] || exit 1
