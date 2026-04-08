#!/usr/bin/env bash
set -euo pipefail

# Smoke-test the config store demo handlers by starting an adapter, running checks,
# and tearing it down automatically.
#
# Usage:
#   ./scripts/smoke_test_config.sh              # defaults to axum
#   ./scripts/smoke_test_config.sh axum
#   ./scripts/smoke_test_config.sh fastly
#   ./scripts/smoke_test_config.sh cloudflare

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEMO_DIR="$ROOT_DIR/examples/app-demo"
ADAPTER="${1:-axum}"
SERVER_PID=""

cleanup() {
  if [ -n "$SERVER_PID" ]; then
    echo ""
    echo "==> Stopping server (PID $SERVER_PID)..."
    pkill -P "$SERVER_PID" 2>/dev/null || true
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# -- Adapter-specific config ------------------------------------------------

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
  cloudflare|cf)
    PORT=8787
    command -v wrangler >/dev/null 2>&1 || {
      echo "wrangler is required. Install with 'npm i -g wrangler'" >&2
      exit 1
    }
    echo "==> Starting Cloudflare wrangler dev on port $PORT..."
    (cd "$DEMO_DIR" && wrangler dev --cwd crates/app-demo-adapter-cloudflare --port "$PORT" 2>&1) &
    SERVER_PID=$!
    ;;
  *)
    echo "Unknown adapter: $ADAPTER" >&2
    echo "Usage: $0 [axum|fastly|cloudflare]" >&2
    exit 1
    ;;
esac

BASE="http://127.0.0.1:${PORT}"

# -- Wait for server readiness ----------------------------------------------

echo "==> Waiting for server at $BASE ..."
MAX_WAIT=60
WAITED=0
until curl -s -o /dev/null "$BASE/" 2>/dev/null; do
  kill -0 "$SERVER_PID" 2>/dev/null || { echo "Server process exited early" >&2; exit 1; }
  sleep 1
  WAITED=$((WAITED + 1))
  if [ "$WAITED" -ge "$MAX_WAIT" ]; then
    echo "Server did not start within ${MAX_WAIT}s" >&2
    exit 1
  fi
done
echo "==> Server ready (${WAITED}s)"

# -- Test helpers ------------------------------------------------------------

PASS=0
FAIL=0

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

section() {
  printf '\n--- %s ---\n' "$1"
}

# -- Tests -------------------------------------------------------------------

section "Health check"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/")
check "GET / returns 200" "200" "$STATUS"

section "Config: keys (all adapters)"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/config/greeting")
check "GET /config/greeting returns 200" "200" "$STATUS"

BODY=$(curl -s "$BASE/config/greeting")
check "greeting value" "hello from config store" "$BODY"

STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/config/feature.new_checkout")
check "GET /config/feature.new_checkout returns 200" "200" "$STATUS"

BODY=$(curl -s "$BASE/config/feature.new_checkout")
check "feature.new_checkout value" "false" "$BODY"

BODY=$(curl -s "$BASE/config/service.timeout_ms")
check "service.timeout_ms value" "1500" "$BODY"

section "Config: missing key returns 404"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/config/does.not.exist")
check "GET /config/does.not.exist returns 404" "404" "$STATUS"

section "Config: case sensitivity"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/config/GREETING")
check "GET /config/GREETING (uppercase) returns 404" "404" "$STATUS"

# -- Summary -----------------------------------------------------------------

printf '\n==============================\n'
printf 'Adapter:  %s\n' "$ADAPTER"
printf 'Results:  %d passed, %d failed\n' "$PASS" "$FAIL"
printf '==============================\n'

[ "$FAIL" -eq 0 ] || exit 1
