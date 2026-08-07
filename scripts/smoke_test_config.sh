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
# Each adapter is seeded with `config push --local`, which writes ONE
# BlobEnvelope holding the whole typed struct under the `app_config`
# key. The assertions therefore exercise `/config/typed` (the
# `AppConfig<AppDemoConfig>` extractor that reads that blob) rather than
# raw per-key `/config/{name}` reads, which have no per-key entries to
# find under this model.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEMO_DIR="$ROOT_DIR/examples/app-demo"
ADAPTER="${1:-axum}"
SERVER_PID=""
# `/config/typed` resolves the demo's mandatory `#[secret] api_token`
# (key name `demo_api_token`) through the AppConfig extractor, so every
# adapter must have that secret seeded before boot or the endpoint errors.
DEMO_SECRET_VALUE="resolved-token"
# Fail-closed backup/restore of the operator-owned files this smoke
# mutates in place. `.dev.vars` is gitignored but NOT regenerable
# (provision writes only empty placeholders), and `fastly.toml`, though
# regenerable, is edited in place by the push and by warm-up.
# shellcheck source=lib/smoke_backup.sh
. "$ROOT_DIR/scripts/lib/smoke_backup.sh"

cleanup() {
  # Kill the server AND its descendants (workerd/spin) and free the port
  # BEFORE restoring, or a survivor could flush state over the restore.
  smoke_stop_server "$SERVER_PID" "${PORT:-}"
  SERVER_PID=""
  restore_backups
}

# Back up operator files BEFORE arming the restore trap and BEFORE warm-up
# (warm-up's `provision --local` and the later `config push` write them).
# A failed backup aborts HERE, before the trap is armed and before any
# mutation, so the tree is never left in a half-restored state.
case "$ADAPTER" in
  axum)
    # `config push --local` / provision write `.edgezero/` (local config
    # JSON + `.env`); back the whole dir up so operator state survives.
    backup_in_tree "$DEMO_DIR/.edgezero"
    ;;
  cloudflare|cf)
    backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-cloudflare/.dev.vars"
    # `config push --local` seeds Miniflare state under `.wrangler/`.
    backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-cloudflare/.wrangler"
    ;;
  fastly)
    backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-fastly/fastly.toml"
    ;;
  spin)
    # `config push --local` writes the SQLite store under `.spin/`.
    backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-spin/.spin"
    ;;
esac

# Install the trap AFTER a successful backup so an abort mid-warm-up still
# restores, while a failed backup aborts before the trap can run. A signal
# runs cleanup then EXITS (the EXIT trap then no-ops on the cleared state),
# so an interrupt can't resume the smoke and re-mutate after restoration.
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

# Warm up per-adapter local state — provision --local synthesises
# wrangler.toml / fastly.toml / spin.toml / runtime-config.toml
# and writes .dev.vars / .env / .edgezero/.env. Fresh clones need
# this because those adapter manifests are gitignored.
# shellcheck source=lib/smoke_warmup.sh
. "$ROOT_DIR/scripts/lib/smoke_warmup.sh"
echo "==> Warming up local state (provision --adapter $ADAPTER --local)..."
smoke_warmup_provision_local "$ADAPTER"

# -- Adapter-specific config ------------------------------------------------

case "$ADAPTER" in
  axum)
    PORT=8787
    echo "==> Building app-demo (axum)..."
    (cd "$DEMO_DIR" && cargo build -p app-demo-adapter-axum 2>&1)
    # Axum reads `.edgezero/local-config-<id>.json`, which
    # `config push --local` writes (see `AxumConfigStore::from_local_file`).
    # `--yes` keeps the push non-interactive (no TTY in CI / warm-up).
    echo "==> Seeding Axum local config store (config push --adapter axum --local)..."
    (cd "$DEMO_DIR" && cargo run -p app-demo-cli --quiet -- \
      config push --adapter axum --local --no-env --yes 2>&1)
    echo "==> Starting Axum adapter on port $PORT..."
    smoke_require_port_free "$PORT"
    # `EnvSecretStore` resolves the `demo_api_token` key verbatim from the
    # process env, so /config/typed's secret walk needs it exported.
    (cd "$DEMO_DIR" && demo_api_token="$DEMO_SECRET_VALUE" \
      cargo run -p app-demo-adapter-axum 2>&1) &
    SERVER_PID=$!
    ;;
  fastly)
    PORT=7676
    command -v fastly >/dev/null 2>&1 || {
      echo "Fastly CLI is required. Install from https://developer.fastly.com/reference/cli/" >&2
      exit 1
    }
    # Seed the local Fastly config store BEFORE `fastly compute serve`.
    # `config push --local` upserts the demo's typed config defaults into
    # `[local_server.config_stores.app_config.contents]` in fastly.toml
    # (the block was created by `provision --local` in the warm-up above).
    # Without this the store is empty and the per-key checks below observe
    # 404s instead of the demo values.
    echo "==> Seeding Fastly local config store (config push --adapter fastly --local)..."
    (cd "$DEMO_DIR" && cargo run -p app-demo-cli --quiet -- \
      config push --adapter fastly --local --no-env --yes 2>&1)
    echo "==> Starting Fastly Viceroy on port $PORT..."
    smoke_require_port_free "$PORT"
    # Warm-up's provision_typed wrote a `[[local_server.secret_stores.default]]`
    # entry mapping `demo_api_token` to the DEMO_API_TOKEN env var; export it
    # so viceroy resolves the secret /config/typed pulls in.
    (cd "$DEMO_DIR" && DEMO_API_TOKEN="$DEMO_SECRET_VALUE" \
      fastly compute serve -C crates/app-demo-adapter-fastly 2>&1) &
    SERVER_PID=$!
    ;;
  cloudflare|cf)
    PORT=8787
    command -v wrangler >/dev/null 2>&1 || {
      echo "wrangler is required. Install with 'npm i -g wrangler'" >&2
      exit 1
    }
    # Seed the local Cloudflare config store BEFORE `wrangler dev`, same
    # as the Fastly arm: `config push --local` writes the demo's typed
    # config defaults into the local KV state wrangler dev reads.
    echo "==> Seeding Cloudflare local config store (config push --adapter cloudflare --local)..."
    (cd "$DEMO_DIR" && cargo run -p app-demo-cli --quiet -- \
      config push --adapter cloudflare --local --no-env --yes 2>&1)
    # wrangler dev does not inherit the shell env into the worker, so the
    # `demo_api_token` secret must come through `.dev.vars`. It was backed
    # up before warm-up (see the pre-warm-up backup block) and is restored
    # on exit.
    printf 'demo_api_token="%s"\n' "$DEMO_SECRET_VALUE" \
      > "$DEMO_DIR/crates/app-demo-adapter-cloudflare/.dev.vars"
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
    # Seed the local Spin KV-backed config store BEFORE `spin up`
    # so the demo's `app_config` label has values to read. Without
    # this, the runtime opens an empty store and the per-key
    # checks below would all observe defaults. `--local` forces the
    # SQLite-direct write into `.spin/sqlite_key_value.db`,
    # bypassing Fermyon Cloud auto-detection; `--no-env` matches
    # the smoke harness shape (no per-key env overlays in play).
    echo "==> Seeding Spin local KV via 'app-demo-cli config push --adapter spin --local --no-env --yes'..."
    (cd "$DEMO_DIR" && cargo run -p app-demo-cli --quiet -- \
      config push --adapter spin --local --no-env --yes 2>&1)
    echo "==> Starting Spin on port $PORT..."
    smoke_require_port_free "$PORT"
    # `--runtime-config-file runtime-config.toml` is REQUIRED — the
    # demo's spin.toml declares non-`default` KV labels
    # (`app_config`, `sessions`, `cache`) and Spin's runtime only
    # auto-provides the `default` label. Without the runtime-config
    # flag, `spin up` aborts with `unknown key_value_stores label
    # <name>` before the server is ready. `SPIN_VARIABLE_DEMO_API_TOKEN`
    # supplies the `demo_api_token` secret /config/typed resolves.
    (cd "$DEMO_DIR/crates/app-demo-adapter-spin" && \
      SPIN_VARIABLE_DEMO_API_TOKEN="$DEMO_SECRET_VALUE" \
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

# `config push` writes ONE BlobEnvelope under the `app_config` key --
# the whole typed struct in a single value. `/config/typed` goes through
# the `AppConfig<AppDemoConfig>` extractor, which reads that blob and
# deserialises it, so this is the endpoint that reflects what push
# actually seeded. (The raw per-key `/config/{name}` reads hit
# `store.get(<name>)` directly; under the blob model there are no
# per-key entries to find, so asserting values there would only pass
# against hand-seeded pre-cutover emulator state.)
section "Config: typed blob (all adapters)"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/config/typed")
check "GET /config/typed returns 200" "200" "$STATUS"

BODY=$(curl -s "$BASE/config/typed")
check "typed greeting value" "hello from app-demo" "$BODY"

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
