#!/usr/bin/env bash
set -euo pipefail

# Smoke-test the KV demo handlers by starting an adapter, running checks,
# and tearing it down automatically.
#
# Usage:
#   ./scripts/smoke_test_kv.sh              # defaults to axum
#   ./scripts/smoke_test_kv.sh axum
#   ./scripts/smoke_test_kv.sh fastly
#   ./scripts/smoke_test_kv.sh cloudflare
#   ./scripts/smoke_test_kv.sh spin

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEMO_DIR="$ROOT_DIR/examples/app-demo"
ADAPTER="${1:-axum}"
SERVER_PID=""

# Fail-closed backup/restore of the operator files + emulator state this
# smoke mutates. Warm-up (`provision --local`) rewrites the manifests /
# `.env` / `.dev.vars`, and the KV pushes seed the persistent emulator
# stores (`.edgezero` / `.wrangler` / `.spin`); without a backup a run
# would leave a developer's tree changed.
# shellcheck source=lib/smoke_backup.sh
. "$ROOT_DIR/scripts/lib/smoke_backup.sh"
# Sourced early (side-effect-free) so the shared provision-owned backup set
# is available before the backup block below. `smoke_backup_provision_local`
# is the single source of truth for which files warm-up can mutate.
# shellcheck source=lib/smoke_warmup.sh
. "$ROOT_DIR/scripts/lib/smoke_warmup.sh"

cleanup() {
  # Kill the server AND its descendants (workerd/spin) and free the port
  # BEFORE restoring, or a survivor could flush state over the restore.
  smoke_stop_server "$SERVER_PID" "${PORT:-}"
  SERVER_PID=""
  restore_backups
}

# Back up BEFORE arming the restore trap and BEFORE warm-up: a failed
# backup aborts here, before any mutation, so the tree is never left in a
# half-restored state. The full provision-owned set (manifests + `.env` /
# `.dev.vars` + emulator-state dirs) is registered by the shared helper in
# scripts/lib/smoke_warmup.sh so this list can't drift from what warm-up and
# the KV pushes actually write.
smoke_backup_provision_local "$ADAPTER"
# Arm the trap only AFTER a successful backup. A signal runs cleanup then
# EXITS so an interrupt can't resume the smoke and re-mutate after restore.
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

# Warm up per-adapter local state — provision --local synthesises
# wrangler.toml / fastly.toml / spin.toml / runtime-config.toml
# and writes .dev.vars / .env / .edgezero/.env. Fresh clones need
# this because those adapter manifests are gitignored. (smoke_warmup.sh is
# already sourced above, alongside the backup set it defines.)
echo "==> Warming up local state (provision --adapter $ADAPTER --local)..."
smoke_warmup_provision_local "$ADAPTER"

# -- Adapter-specific config ------------------------------------------------

case "$ADAPTER" in
  axum)
    PORT=8787
    echo "==> Building app-demo (axum)..."
    (cd "$DEMO_DIR" && cargo build -p app-demo-adapter-axum 2>&1)
    echo "==> Starting Axum adapter on port $PORT..."
    smoke_require_port_free "$PORT"
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
    smoke_require_port_free "$PORT"
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
    smoke_require_port_free "$PORT"
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
    smoke_require_port_free "$PORT"
    # `--runtime-config-file runtime-config.toml`: the demo's
    # spin.toml declares non-`default` KV labels (`sessions`,
    # `cache`) and Spin's runtime only auto-provides the `default`
    # label. Without the flag, `spin up` aborts with `unknown
    # key_value_stores label <name>` before the server is ready.
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
    printf '  FAIL  %s  (expected %s, got %s)\n' "$label" "$expect" "$actual"
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

section "KV Counter"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/kv/counter")
check "POST /kv/counter returns 200" "200" "$STATUS"

BODY=$(curl -s -X POST "$BASE/kv/counter")
FIRST_COUNT=$(echo "$BODY" | grep -o '"count":[0-9]*' | head -1 | cut -d: -f2)
BODY=$(curl -s -X POST "$BASE/kv/counter")
SECOND_COUNT=$(echo "$BODY" | grep -o '"count":[0-9]*' | head -1 | cut -d: -f2)
check \
  "Counter increments" \
  "true" \
  "$([ -n "$FIRST_COUNT" ] && [ -n "$SECOND_COUNT" ] && [ "$SECOND_COUNT" -eq $((FIRST_COUNT + 1)) ] 2>/dev/null && echo true || echo false)"

section "KV Notes: PUT + GET"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/kv/notes/smoke-test" -d "hello from smoke test")
check "POST /kv/notes/smoke-test returns 201" "201" "$STATUS"

BODY=$(curl -s "$BASE/kv/notes/smoke-test")
check "GET /kv/notes/smoke-test returns note" "hello from smoke test" "$BODY"

section "KV Notes: DELETE"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/kv/notes/smoke-test")
check "DELETE /kv/notes/smoke-test returns 204" "204" "$STATUS"

STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/kv/notes/smoke-test")
check "GET deleted note returns 404" "404" "$STATUS"

section "KV Notes: GET missing key"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/kv/notes/does-not-exist")
check "GET /kv/notes/does-not-exist returns 404" "404" "$STATUS"

# -- Summary -----------------------------------------------------------------

printf '\n==============================\n'
printf 'Adapter:  %s\n' "$ADAPTER"
printf 'Results:  %d passed, %d failed\n' "$PASS" "$FAIL"
printf '==============================\n'

[ "$FAIL" -eq 0 ] || exit 1
