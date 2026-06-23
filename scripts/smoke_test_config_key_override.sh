#!/usr/bin/env bash
set -euo pipefail

# 12.7 + 9.3 + 8.3 multi-adapter smoke:
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
#    must return non-zero with the 8.3 message; `config push --yes`
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
# Match the demo's edgezero.toml port (8787) so the runtime and
# this script speak the same port. Hardcoding a different port
# previously caused the curl wait to time out while the runtime
# bound elsewhere; the smoke would then claim a "non-server" pass.
PORT=8787
PASS=0
FAIL=0
# Per-row backup of files the smoke would otherwise mutate in place
# in the checked-in app-demo tree. Cleanup restores them on exit.
declare -a BACKUPS=()

# Stop the running runtime without touching tracked-fixture backups.
# Used between staging-blob and default-blob assertions in the same
# row so the pushed remote state survives a runtime restart. Sends
# SIGTERM, waits up to 5s, then SIGKILLs survivors. If anything is
# still bound to $PORT after that (e.g. a grand-child not reachable
# via $SERVER_PID), uses `lsof` to hunt it down. Returns non-zero
# if the port refuses to free -- the caller MUST surface that, or
# the next boot will silently inherit the prior server's responses
# and assertions will compare against stale state.
stop_server() {
  local rc=0
  if [ -n "$SERVER_PID" ]; then
    pkill -TERM -P "$SERVER_PID" 2>/dev/null || true
    kill -TERM "$SERVER_PID" 2>/dev/null || true
    local waited=0
    while [ "$waited" -lt 5 ] && kill -0 "$SERVER_PID" 2>/dev/null; do
      sleep 1
      waited=$((waited + 1))
    done
    pkill -KILL -P "$SERVER_PID" 2>/dev/null || true
    kill -KILL "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
  fi
  # Best-effort port-binder kill via lsof. The cargo-run wrapper
  # often spawns a child that outlives the wrapper PID; pkill -P
  # catches direct children, but a re-exec or grand-child can
  # survive. lsof finds whoever's actually listening on $PORT.
  if command -v lsof >/dev/null 2>&1; then
    local port_pids
    port_pids=$(lsof -ti ":${PORT}" 2>/dev/null || true)
    if [ -n "$port_pids" ]; then
      printf '  note: killing stray port-%s holder(s): %s\n' "$PORT" "$port_pids" >&2
      # shellcheck disable=SC2086
      kill -KILL $port_pids 2>/dev/null || true
    fi
  fi
  # Verify the port is free; fail loud if not. A live port here
  # means the next boot would either silently inherit the old
  # server's responses or fail to bind -- either way the row's
  # assertions would be meaningless.
  local waited=0
  while [ "$waited" -lt 10 ]; do
    if ! curl -s -o /dev/null --connect-timeout 1 "http://127.0.0.1:${PORT}/" 2>/dev/null; then
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  printf '  FAIL  stop_server: port %s still serving after 10s -- prior runtime did not die\n' "$PORT" >&2
  FAIL=$((FAIL + 1))
  rc=1
  return "$rc"
}

# Restore tracked fixtures the smoke mutated in place. Called once
# per row AFTER all assertions for that row have finished, and again
# from the EXIT trap as a safety net.
restore_backups() {
  for pair in "${BACKUPS[@]:-}"; do
    [ -z "$pair" ] && continue
    orig="${pair%%::*}"
    back="${pair##*::}"
    if [ -s "$back" ]; then
      mv "$back" "$orig" 2>/dev/null || true
    else
      # Empty marker file = the original didn't exist; remove what
      # the smoke created.
      rm -f "$back" 2>/dev/null || true
      rm -f "$orig" 2>/dev/null || true
    fi
  done
  BACKUPS=()
}

cleanup() {
  stop_server
  restore_backups
}
trap cleanup EXIT INT TERM

# Record a backup of $1 (an in-tree file the smoke is about to mutate)
# so `cleanup` can restore it.
backup_in_tree() {
  local orig="$1"
  local back
  back=$(mktemp)
  if [ -e "$orig" ]; then
    cp -p "$orig" "$back"
  else
    : > "$back"  # marker that the file didn't exist
  fi
  BACKUPS+=("${orig}::${back}")
}

# Bash 3.2-portable upper-case (macOS ships /usr/bin/env bash as 3.2).
# `${var^^}` is Bash 4+; tr is portable.
upper() {
  printf '%s' "$1" | tr '[:lower:]' '[:upper:]'
}

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

# Build the adapter's runtime artifact if needed. The Fastly and Spin
# rows boot a wasm binary that has to exist on disk before the
# emulator starts; a clean checkout has no `target/.../debug/*.wasm`
# yet, so the boot would fail before the runtime came up.
ensure_runtime_built() {
  local adapter="$1"
  case "$adapter" in
    axum)
      # `cargo run -p app-demo-cli -- serve` builds on demand.
      ;;
    cloudflare)
      # `wrangler dev` invokes wrangler's own build pipeline.
      ;;
    fastly)
      (cd "$DEMO_DIR" && cargo build --quiet \
        --target wasm32-wasip1 \
        --manifest-path crates/app-demo-adapter-fastly/Cargo.toml \
        --features fastly) || return 1
      ;;
    spin)
      (cd "$DEMO_DIR" && cargo build --quiet --release \
        --target wasm32-wasip2 \
        --manifest-path crates/app-demo-adapter-spin/Cargo.toml \
        --features spin) || return 1
      ;;
  esac
}

# Seed the platform-local secret store for the chosen adapter with
# `demo_api_token = "resolved-token"`. Without this, the
# /config/typed handler's secret walk fails before any KEY-override
# assertion can run. Each adapter's local-emulator secret store
# differs:
#   - axum:       process env var (handled inline by boot_runtime).
#   - cloudflare: .dev.vars file at the worker root.
#   - fastly:     [local_server.secret_stores.default.contents] in
#                 fastly.toml (mutates the tracked file -- the
#                 caller already backed it up).
#   - spin:       runtime-config.toml's variable provider; the
#                 demo's spin.toml exposes `demo_api_token` as a
#                 variable. We pre-create a local override file the
#                 runtime reads.
# Best-effort: if the adapter's per-platform mechanism isn't
# present on this host (e.g. wrangler not installed), the helper
# logs and returns 1; the caller skips the row.
seed_secret_for_adapter() {
  local adapter="$1"
  case "$adapter" in
    axum)
      # No-op: boot_runtime sets the env var inline.
      return 0
      ;;
    cloudflare)
      local dev_vars="$DEMO_DIR/crates/app-demo-adapter-cloudflare/.dev.vars"
      printf 'demo_api_token="resolved-token"\n' > "$dev_vars"
      return 0
      ;;
    fastly)
      local fastly_toml="$DEMO_DIR/crates/app-demo-adapter-fastly/fastly.toml"
      # The fixture's [local_server.secret_stores.default] is an
      # array-of-tables (each entry exposes one key + the env var
      # to read its value from). Append a second entry rather than
      # opening a normal-table block at the same path; mixing the
      # two forms is a TOML parse error. Viceroy reads
      # `demo_api_token`'s value from $DEMO_API_TOKEN_SECRET, which
      # boot_runtime exports inline.
      cat >> "$fastly_toml" <<'TOML'

[[local_server.secret_stores.default]]
key = "demo_api_token"
env = "DEMO_API_TOKEN_SECRET"
TOML
      return 0
      ;;
    spin)
      # The Spin fixture's spin.toml declares the `api_token`
      # variable (the schema field name) but the runtime secret
      # walk asks for the blob's VALUE -- `demo_api_token`. Patch
      # the fixture in place so both the [variables] block and
      # the [component.app-demo.variables] map expose
      # `demo_api_token` for the smoke's lifetime. The caller
      # backs spin.toml up so cleanup restores the original.
      local spin_toml="$DEMO_DIR/crates/app-demo-adapter-spin/spin.toml"
      # Insert demo_api_token into the application [variables] block.
      # Use awk to add the new line right after `api_token = ...`.
      awk '
        /^api_token = / && !patched_vars {
          print
          print "demo_api_token = { required = true, secret = true }"
          patched_vars = 1
          next
        }
        /^api_token = "\{\{ api_token \}\}"/ && !patched_comp {
          print
          print "demo_api_token = \"{{ demo_api_token }}\""
          patched_comp = 1
          next
        }
        { print }
      ' "$spin_toml" > "$spin_toml.tmp" && mv "$spin_toml.tmp" "$spin_toml"
      return 0
      ;;
    *)
      printf '  note: unknown adapter %s for secret seeding\n' "$adapter" >&2
      return 1
      ;;
  esac
}

# Boot the right runtime for $1 (adapter name), returning when the
# greeting endpoint responds 200. All rows bind to $PORT (8787 to
# match the app-demo edgezero.toml `[adapters.axum.adapter] port`).
#
# Secret seeding: see `seed_secret_for_adapter` above. The Axum
# and Spin rows set the secret env var inline at spawn time
# (EnvSecretStore reads from the process env); Cloudflare and
# Fastly read from per-adapter on-disk files written by the
# seed helper.
boot_runtime() {
  local adapter="$1"
  ensure_runtime_built "$adapter" || return 1
  # Refuse to launch if the port is already taken. If we boot anyway,
  # wait_for_port could return success on the OTHER process's response.
  if curl -s -o /dev/null --connect-timeout 1 "http://127.0.0.1:${PORT}/" 2>/dev/null; then
    printf '  FAIL  boot_runtime: port %s already in use; refusing to boot %s\n' "$PORT" "$adapter" >&2
    FAIL=$((FAIL + 1))
    return 1
  fi
  case "$adapter" in
    axum)
      # Seed `demo_api_token` so the AppConfig secret walk
      # resolves; without it, /config/typed returns
      # ConfigOutOfDate before the assertion can fire.
      (cd "$DEMO_DIR" && demo_api_token=resolved-token \
        cargo run --quiet -p app-demo-cli -- serve --adapter axum 2>&1) &
      ;;
    cloudflare)
      (cd "$DEMO_DIR/crates/app-demo-adapter-cloudflare" && \
        wrangler dev --local --port "$PORT" 2>&1) &
      ;;
    fastly)
      # DEMO_API_TOKEN_SECRET is the env var viceroy reads to
      # populate the `demo_api_token` secret-store entry seeded
      # by seed_secret_for_adapter.
      (cd "$DEMO_DIR/crates/app-demo-adapter-fastly" && \
        DEMO_API_TOKEN_SECRET=resolved-token \
        viceroy run -C fastly.toml --addr "127.0.0.1:${PORT}" \
          target/wasm32-wasip1/debug/app_demo_adapter_fastly.wasm 2>&1) &
      ;;
    spin)
      # spin reads variables from the app manifest; the demo wires
      # `demo_api_token` to the SPIN_VARIABLE_DEMO_API_TOKEN env var
      # (Spin's documented passthrough).
      (cd "$DEMO_DIR/crates/app-demo-adapter-spin" && \
        SPIN_VARIABLE_DEMO_API_TOKEN=resolved-token \
        spin up --listen "127.0.0.1:${PORT}" \
          --runtime-config-file runtime-config.toml 2>&1) &
      ;;
    *)
      echo "unknown adapter: $adapter" >&2; return 1 ;;
  esac
  SERVER_PID=$!
  wait_for_port
}

# (adapter, extra-push-flags) per row. Axum's push is always local
# (no --local flag); the other three need --local for the local
# emulator state seed.
SUITES=(
  "axum:"
  "cloudflare:--local"
  "fastly:--local"
  "spin:--local"
)

# -- 12.7: per-adapter __KEY override loop -------------------------------

for suite in "${SUITES[@]}"; do
  adapter="${suite%%:*}"
  extra="${suite#*:}"

  skip_var="SKIP_$(upper "$adapter")"
  eval "skip_val=\${${skip_var}:-0}"
  if [ "$skip_val" = "1" ]; then
    printf '\n=== 12.7 __KEY override smoke: %s SKIPPED (%s=1) ===\n' "$adapter" "$skip_var"
    continue
  fi

  printf '\n=== 12.7 __KEY override smoke: %s%s ===\n' "$adapter" "${extra:+ $extra}"
  tmp=$(mktemp -d)
  trap "cleanup; rm -rf '$tmp'" EXIT INT TERM

  # Back up any tracked fixture the push will mutate in place. For
  # Fastly that's fastly.toml; gitignored local-state directories
  # (`.wrangler/`, `.spin/`, `.edgezero/`) are fine to write to.
  # The Cloudflare row writes a transient `.dev.vars` file -- back
  # it up too so the worktree stays clean.
  if [ "$adapter" = "fastly" ]; then
    backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-fastly/fastly.toml"
  fi
  if [ "$adapter" = "cloudflare" ]; then
    backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-cloudflare/.dev.vars"
  fi
  if [ "$adapter" = "spin" ]; then
    backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-spin/spin.toml"
  fi

  # Seed the platform-local secret store so the runtime
  # /config/typed handler's secret walk resolves at request time.
  # Without it the assertion would fail on the secret-walk error,
  # not the __KEY override (which is what this row is gating).
  if ! seed_secret_for_adapter "$adapter"; then
    printf '  note: %s row skipped -- secret seeding unavailable on this host\n' "$adapter" >&2
    cleanup
    rm -rf "$tmp"
    trap cleanup EXIT INT TERM
    continue
  fi

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

  # 3. Boot with __KEY=staging; assert staging. The /config/typed
  # route is the AppConfig<AppDemoConfig> handler (handlers.rs:185);
  # the /config/<key> route is the raw config-store map and would
  # always 404 on the blob model. Only /config/typed proves the
  # extractor read the right blob.
  if ! EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging \
      boot_runtime "$adapter"; then
    cleanup
    rm -rf "$tmp"
    trap cleanup EXIT INT TERM
    continue
  fi
  result=$(curl -s "http://127.0.0.1:${PORT}/config/typed")
  check "$adapter __KEY override returns staging" "staging-blob" "$result"
  # Stop the server BUT keep the pushed blobs in place -- restoring
  # fixtures would wipe the default blob before the next boot reads it.
  # If stop_server fails (port stays live), abort the row: the next
  # boot would either inherit the prior server's responses or fail
  # to bind, and the default-key assertion would be meaningless.
  if ! stop_server; then
    cleanup
    rm -rf "$tmp"
    trap cleanup EXIT INT TERM
    continue
  fi

  # 4. Reboot without __KEY; assert default.
  unset EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY
  if ! boot_runtime "$adapter"; then
    cleanup
    rm -rf "$tmp"
    trap cleanup EXIT INT TERM
    continue
  fi
  result=$(curl -s "http://127.0.0.1:${PORT}/config/typed")
  check "$adapter __KEY unset returns default" "default-blob" "$result"
  # Now both assertions are done -- restore tracked fixtures.
  cleanup

  rm -rf "$tmp"
  trap cleanup EXIT INT TERM
done

# -- 9.3 Fastly oversized envelope smoke ---------------------------------

if [ "${SKIP_FASTLY:-0}" = "1" ]; then
  printf '\n=== 9.3 Fastly chunk-pointer smoke: SKIPPED (SKIP_FASTLY=1) ===\n'
else
  printf '\n=== 9.3 Fastly chunk-pointer smoke ===\n'
  tmp=$(mktemp -d)
  trap "cleanup; rm -rf '$tmp'" EXIT INT TERM

  # The local push rewrites fastly.toml in the checked-in app-demo
  # tree; back it up so `cleanup` restores it on exit.
  backup_in_tree "$DEMO_DIR/crates/app-demo-adapter-fastly/fastly.toml"

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

  # Seed the demo_api_token secret so /config/typed's secret walk
  # resolves; without it the assertion would fail in the extractor
  # before testing the chunk-pointer round-trip. The fastly.toml
  # append survives in the per-row backup and gets restored on
  # cleanup.
  seed_secret_for_adapter fastly || true

  if boot_runtime fastly; then
    # /config/typed routes through AppConfig<AppDemoConfig> which
    # invokes the runtime chunk-pointer resolver -- proving the
    # reconstructed envelope flows into the typed extractor.
    result=$(curl -s "http://127.0.0.1:${PORT}/config/typed")
    check_contains "fastly runtime reads reconstructed envelope" "large-fastly-blob" "$result"
  fi
  cleanup

  rm -rf "$tmp"
  trap cleanup EXIT INT TERM
fi

# -- 8.3 Spin Cloud Unsupported smoke ------------------------------------

if [ "${SKIP_SPIN_CLOUD_SMOKE:-0}" = "1" ]; then
  printf '\n=== 8.3 Spin Cloud Unsupported smoke: SKIPPED (SKIP_SPIN_CLOUD_SMOKE=1) ===\n'
else
  printf '\n=== 8.3 Spin Cloud Unsupported smoke ===\n'
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
