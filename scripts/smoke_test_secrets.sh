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
# Operator-owned files/dirs this smoke mutates. `SMOKE_SECRET` is a
# smoke-only allowlisted secret (NOT a typed config field), so provision
# never declares it: the Fastly and Spin arms inject the declaration into
# the generated fastly.toml / spin.toml before boot, and warm-up's
# `provision --local` rewrites `.dev.vars` / `.edgezero`. All of these are
# backed up FAIL-CLOSED BEFORE warm-up and restored on exit, so a
# developer's tree is never changed regardless of success/failure.
# shellcheck source=lib/smoke_backup.sh
. "$ROOT_DIR/scripts/lib/smoke_backup.sh"
# Path vars the seed arms below write to (set in the backup case block).
DEV_VARS_FILE=""
FASTLY_TOML_FILE=""
SPIN_TOML_FILE=""
SMOKE_SECRET_NAME="SMOKE_SECRET"
MISSING_SECRET_NAME="SMOKE_SECRET_MISSING"
DISALLOWED_SECRET_NAME="API_KEY"
SMOKE_SECRET_VALUE="smoke-secret-$(date +%s)-$$"
PASS=0
FAIL=0

export SMOKE_SECRET="$SMOKE_SECRET_VALUE"

cleanup() {
  # Kill the server AND its descendants (workerd/spin) and free the port
  # BEFORE restoring, or a survivor could flush state over the restore.
  smoke_stop_server "$SERVER_PID" "${PORT:-}"
  SERVER_PID=""
  restore_backups
}

# Back up operator files/dirs BEFORE arming the restore trap and BEFORE
# warm-up (and the boot-time seeds) mutate them. A failed backup aborts
# HERE -- before the trap is armed and before any mutation -- so the tree
# is never left half-restored.
case "$ADAPTER" in
  cloudflare)
    DEV_VARS_FILE="$DEMO_DIR/crates/app-demo-adapter-cloudflare/.dev.vars"
    backup_in_tree "$DEV_VARS_FILE"
    ;;
  fastly)
    FASTLY_TOML_FILE="$DEMO_DIR/crates/app-demo-adapter-fastly/fastly.toml"
    backup_in_tree "$FASTLY_TOML_FILE"
    ;;
  spin)
    SPIN_TOML_FILE="$DEMO_DIR/crates/app-demo-adapter-spin/spin.toml"
    backup_in_tree "$SPIN_TOML_FILE"
    ;;
  axum)
    backup_in_tree "$DEMO_DIR/.edgezero"
    ;;
esac

# Install the trap AFTER a successful backup so an abort mid-warm-up still
# restores, while a failed backup aborts before the trap can run. A signal
# runs cleanup then EXITS so an interrupt can't resume and re-mutate.
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

# Warm up per-adapter local state — provision --local synthesises
# wrangler.toml / fastly.toml / spin.toml / runtime-config.toml
# and writes .dev.vars / .env / .edgezero/.env. Fresh clones need
# this because those adapter manifests are gitignored. Crucial for
# this smoke: the typed dispatch writes SPIN_VARIABLE_* /
# .dev.vars placeholders that the emulator boot reads.
# shellcheck source=lib/smoke_warmup.sh
. "$ROOT_DIR/scripts/lib/smoke_warmup.sh"
echo "==> Warming up local state (provision --adapter $ADAPTER --local)..."
smoke_warmup_provision_local "$ADAPTER"

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
      # Viceroy resolves `SMOKE_SECRET` verbatim from a
      # `[[local_server.secret_stores.default]]` entry. Provision only
      # declares typed secrets (demo_api_token), so inject this one and
      # map it to the exported $SMOKE_SECRET env var. fastly.toml was
      # backed up before warm-up; cleanup restores it.
      cat >> "$FASTLY_TOML_FILE" <<'TOML'

[[local_server.secret_stores.default]]
key = "SMOKE_SECRET"
env = "SMOKE_SECRET"
TOML
      echo "==> Starting Fastly Viceroy on port $PORT..."
      smoke_require_port_free "$PORT"
      (cd "$DEMO_DIR" && fastly compute serve -C crates/app-demo-adapter-fastly 2>&1) &
      SERVER_PID=$!
      ;;
    cloudflare)
      PORT=8787
      command -v wrangler >/dev/null 2>&1 || {
        echo "wrangler is required. Install with 'npm i -g wrangler'" >&2
        exit 1
      }
      # `.dev.vars` was backed up before warm-up; cleanup restores it.
      printf '%s=%s\n' "$SMOKE_SECRET_NAME" "$SMOKE_SECRET_VALUE" > "$DEV_VARS_FILE"
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
      # SpinSecretStore lowercases the key, so `SMOKE_SECRET` resolves the
      # `smoke_secret` Spin variable. Provision only declares typed secrets,
      # so inject the variable declaration + a component binding into the
      # generated spin.toml before boot.
      # spin.toml was backed up before warm-up; cleanup restores it.
      python3 - "$SPIN_TOML_FILE" <<'PY'
import re
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as fh:
    text = fh.read()


def section_body(source, header_regex):
    """Return (match, body) for the table `header_regex` names, where
    body is the text between that header and the next `[` header."""
    match = header_regex.search(source)
    if match is None:
        return None, ""
    rest = source[match.end():]
    nxt = re.search(r'(?m)^\[', rest)
    return match, (rest[:nxt.start()] if nxt else rest)


def insert_under(source, header_regex, line):
    """Insert `line` immediately after the header the regex matches."""
    return header_regex.sub(lambda m: m.group(0) + "\n" + line.rstrip("\n"),
                            source, count=1)


# 1. Declare the top-level variable (Spin requires it before a component
#    can reference it). Insert under an existing `[variables]` table, or
#    create one if the manifest has no typed secrets.
vars_re = re.compile(r'(?m)^\[variables\]\s*$')
_, vars_text = section_body(text, vars_re)
var_line = 'smoke_secret = { required = true }\n'
if "smoke_secret" not in vars_text:
    if vars_re.search(text):
        text = insert_under(text, vars_re, var_line)
    else:
        text += "\n[variables]\n" + var_line

# 2. Bind it on the single component so the runtime exposes it. The demo
#    ships exactly one `[component.<id>]`.
comp = re.search(r'(?m)^\[component\.([A-Za-z0-9_-]+)\]\s*$', text)
if comp is None:
    sys.exit("no [component.<id>] found in spin.toml")
cid = comp.group(1)
bind_re = re.compile(r'(?m)^\[component\.' + re.escape(cid) + r'\.variables\]\s*$')
_, bind_text = section_body(text, bind_re)
bind_line = 'smoke_secret = "{{ smoke_secret }}"\n'
if "smoke_secret" not in bind_text:
    if bind_re.search(text):
        text = insert_under(text, bind_re, bind_line)
    else:
        text += f"\n[component.{cid}.variables]\n" + bind_line

with open(path, "w", encoding="utf-8") as fh:
    fh.write(text)
PY
      echo "==> Starting Spin on port $PORT..."
      smoke_require_port_free "$PORT"
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
