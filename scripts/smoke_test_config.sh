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
#   ./scripts/smoke_test_config.sh spin
#
# Note (spin): handler-facing dotted keys (`feature.new_checkout`,
# `service.timeout_ms`) are supported on Spin too; `SpinConfigStore`
# translates them to the flat variable form (`feature__new_checkout`,
# `service__timeout_ms`) before lookup.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEMO_DIR="$ROOT_DIR/examples/app-demo"
ADAPTER="${1:-axum}"
SERVER_PID=""

# Warm up per-adapter local state — provision --local synthesises
# wrangler.toml / fastly.toml / spin.toml / runtime-config.toml
# and writes .dev.vars / .env / .edgezero/.env. Fresh clones need
# this because Task 33 gitignored those files.
# shellcheck source=lib/smoke_warmup.sh
. "$ROOT_DIR/scripts/lib/smoke_warmup.sh"
echo "==> Warming up local state (provision --adapter $ADAPTER --local)..."
smoke_warmup_provision_local "$ADAPTER"

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
    # Stage 2 Axum config is read from `.edgezero/local-config-<id>.json`
    # per logical id (see `AxumConfigStore::from_local_file`). `config push`
    # writes that file in Stage 7; until then the smoke script seeds it
    # directly with the same demo values Fastly's
    # `[local_server.config_stores.app_config.contents]` carries (and that
    # `config push --adapter spin --local` will write directly into
    # `.spin/sqlite_key_value.db` once the per-backend writer lands).
    echo "==> Seeding .edgezero/local-config-app_config.json..."
    mkdir -p "$DEMO_DIR/.edgezero"
    cat > "$DEMO_DIR/.edgezero/local-config-app_config.json" <<'JSON'
{
  "greeting": "hello from config store",
  "feature.new_checkout": "false",
  "service.timeout_ms": "1500"
}
JSON
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
  spin)
    PORT=3000
    command -v spin >/dev/null 2>&1 || {
      echo "Spin CLI is required. Install from https://developer.fermyon.com/spin/v3/install" >&2
      exit 1
    }
    echo "==> Building Spin WASM (wasm32-wasip2)..."
    (cd "$DEMO_DIR" && cargo build --target wasm32-wasip2 --release -p app-demo-adapter-spin 2>&1)
    # Seed the local Spin KV-backed config store BEFORE `spin up`
    # so the demo's `app_config` label has values to read. Without
    # this, the runtime opens an empty store and the per-key
    # checks below would all observe defaults. `--local` forces the
    # SQLite-direct write into `.spin/sqlite_key_value.db`,
    # bypassing Fermyon Cloud auto-detection; `--no-env` matches
    # the smoke harness shape (no per-key env overlays in play).
    echo "==> Seeding Spin local KV via 'app-demo-cli config push --adapter spin --local --no-env'..."
    (cd "$DEMO_DIR" && cargo run -p app-demo-cli --quiet -- \
      config push --adapter spin --local --no-env 2>&1)
    echo "==> Starting Spin on port $PORT..."
    # `--runtime-config-file runtime-config.toml` is REQUIRED — the
    # demo's spin.toml declares non-`default` KV labels
    # (`app_config`, `sessions`, `cache`) and Spin's runtime only
    # auto-provides the `default` label. Without the runtime-config
    # flag, `spin up` aborts with `unknown key_value_stores label
    # <name>` before the server is ready.
    (cd "$DEMO_DIR/crates/app-demo-adapter-spin" && \
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
