#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd)
ACTION_DIR="$ROOT/.github/actions/deploy"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

export EDGEZERO_RUNNER_OS=Linux
export EDGEZERO_RUNNER_ARCH=X64

run_expect_fail() {
  local name="$1"
  shift
  set +e
  "$@" >"$TMP/$name.out" 2>"$TMP/$name.err"
  local status=$?
  set -e
  [[ $status -ne 0 ]] || fail "$name unexpectedly succeeded"
}

run_expect_ok() {
  local name="$1"
  shift
  "$@" >"$TMP/$name.out" 2>"$TMP/$name.err" || {
    cat "$TMP/$name.out" >&2 || true
    cat "$TMP/$name.err" >&2 || true
    fail "$name failed"
  }
}

# action metadata parses, exposes the expected v0 API, and clears provider env in non-deploy steps.
ruby - "$ACTION_DIR/action.yml" <<'RUBY'
require "yaml"
y = YAML.load_file(ARGV[0])
abort "not composite" unless y["runs"]["using"] == "composite"
abort "missing adapter input" unless y["inputs"].key?("adapter")
abort "unexpected cloudflare input" if y["inputs"].key?("cloudflare-api-token")
abort "unexpected fermyon input" if y["inputs"].key?("fermyon-cloud-token")
aliases = %w[
  FASTLY_API_TOKEN FASTLY_SERVICE_ID FASTLY_TOKEN FASTLY_PROFILE
  FASTLY_API_ENDPOINT FASTLY_ENDPOINT FASTLY_API_URL FASTLY_DEBUG
  FASTLY_DEBUG_MODE FASTLY_CONFIG_FILE FASTLY_HOME FASTLY_SERVICE_NAME
  FASTLY_API_KEY FASTLY_AUTH_TOKEN INPUT_FASTLY_API_TOKEN
]
y["runs"]["steps"].each do |step|
  next if step["name"] == "Deploy"
  env = step["env"] || {}
  aliases.each do |name|
    abort "#{step["name"]} does not clear #{name}" unless env[name] == ""
  end
end
RUBY

# validate-inputs rejects unsupported runners early.
GITHUB_OUTPUT="$TMP/reject-runner.out" \
EDGEZERO_RUNNER_OS=macOS \
EDGEZERO_RUNNER_ARCH=X64 \
INPUT_ADAPTER=fastly \
INPUT_FASTLY_API_TOKEN_PRESENT=true \
INPUT_FASTLY_SERVICE_ID=svc \
run_expect_fail reject-runner "$ACTION_DIR/scripts/validate-inputs.sh"
grep -q "Linux x86-64" "$TMP/reject-runner.err" || fail "runner rejection message missing"

# validate-inputs accepts fastly and writes parsed args without logging values.
GITHUB_OUTPUT="$TMP/validate.out" \
EDGEZERO_ACTION_STATE_DIR="$TMP/state" \
INPUT_ADAPTER=fastly \
INPUT_BUILD_MODE=auto \
INPUT_CACHE=false \
INPUT_BUILD_ARGS='["--feature", "value with space"]' \
INPUT_DEPLOY_ARGS='["--comment", "safe"]' \
INPUT_FASTLY_API_TOKEN_PRESENT=true \
INPUT_FASTLY_SERVICE_ID=svc123 \
run_expect_ok validate "$ACTION_DIR/scripts/validate-inputs.sh"
grep -q '^adapter=fastly$' "$TMP/validate.out" || fail "validate did not output adapter"

# validate-inputs rejects unsupported and future adapters.
GITHUB_OUTPUT="$TMP/reject.out" INPUT_ADAPTER=cloudflare INPUT_FASTLY_API_TOKEN_PRESENT=true INPUT_FASTLY_SERVICE_ID=svc run_expect_fail reject-cloudflare "$ACTION_DIR/scripts/validate-inputs.sh"
grep -q "not implemented in v0" "$TMP/reject-cloudflare.err" || fail "cloudflare rejection message missing"
GITHUB_OUTPUT="$TMP/reject-axum.out" INPUT_ADAPTER=axum INPUT_FASTLY_API_TOKEN_PRESENT=true INPUT_FASTLY_SERVICE_ID=svc run_expect_fail reject-axum "$ACTION_DIR/scripts/validate-inputs.sh"
grep -q "no EdgeZero remote deployment contract" "$TMP/reject-axum.err" || fail "axum rejection message missing"
GITHUB_OUTPUT="$TMP/reject-spin.out" INPUT_ADAPTER=spin INPUT_FASTLY_API_TOKEN_PRESENT=true INPUT_FASTLY_SERVICE_ID=svc run_expect_fail reject-spin "$ACTION_DIR/scripts/validate-inputs.sh"
grep -q "not implemented in v0" "$TMP/reject-spin.err" || fail "spin rejection message missing"
GITHUB_OUTPUT="$TMP/reject-unknown.out" INPUT_ADAPTER=banana INPUT_FASTLY_API_TOKEN_PRESENT=true INPUT_FASTLY_SERVICE_ID=svc run_expect_fail reject-unknown "$ACTION_DIR/scripts/validate-inputs.sh"
grep -q "unsupported adapter" "$TMP/reject-unknown.err" || fail "unknown adapter rejection message missing"

# validate-inputs rejects dangerous Fastly override flags.
GITHUB_OUTPUT="$TMP/reject-service.out" \
EDGEZERO_ACTION_STATE_DIR="$TMP/state2" \
INPUT_ADAPTER=fastly \
INPUT_FASTLY_API_TOKEN_PRESENT=true \
INPUT_FASTLY_SERVICE_ID=svc \
INPUT_DEPLOY_ARGS='["--service-id", "other"]' \
run_expect_fail reject-service "$ACTION_DIR/scripts/validate-inputs.sh"
grep -q "deploy-args allows only Fastly comment flags" "$TMP/reject-service.err" || fail "dangerous flag rejection missing"
GITHUB_OUTPUT="$TMP/reject-interactive.out" \
EDGEZERO_ACTION_STATE_DIR="$TMP/state3" \
INPUT_ADAPTER=fastly \
INPUT_FASTLY_API_TOKEN_PRESENT=true \
INPUT_FASTLY_SERVICE_ID=svc \
INPUT_DEPLOY_ARGS='["--non-interactive=false"]' \
run_expect_fail reject-interactive "$ACTION_DIR/scripts/validate-inputs.sh"
grep -q "deploy-args allows only Fastly comment flags" "$TMP/reject-interactive.err" || fail "non-interactive override rejection missing"
GITHUB_OUTPUT="$TMP/reject-short-service.out" \
EDGEZERO_ACTION_STATE_DIR="$TMP/state4" \
INPUT_ADAPTER=fastly \
INPUT_FASTLY_API_TOKEN_PRESENT=true \
INPUT_FASTLY_SERVICE_ID=svc \
INPUT_DEPLOY_ARGS='["-s", "other"]' \
run_expect_fail reject-short-service "$ACTION_DIR/scripts/validate-inputs.sh"
grep -q "deploy-args allows only Fastly comment flags" "$TMP/reject-short-service.err" || fail "short service override rejection missing"

# annotations escape percent and newlines.
run_expect_fail escaped-annotation bash -c "source '$ACTION_DIR/scripts/common.sh'; fail \$'bad%line\nnext'"
grep -q '::error::bad%25line%0Anext' "$TMP/escaped-annotation.err" || fail "annotation escaping missing"

# resolve-project discovers app state, manifest, source revision, and fastly auto build mode.
APP="$TMP/workspace/app"
mkdir -p "$APP"
git -C "$TMP/workspace" init -q
git -C "$TMP/workspace" config user.email test@example.com
git -C "$TMP/workspace" config user.name Test
git -C "$TMP/workspace" config commit.gpgsign false
cat >"$TMP/workspace/Cargo.lock" <<'LOCK'
# fixture lockfile
LOCK
cat >"$APP/edgezero.toml" <<'TOML'
[adapters.fastly.commands]
deploy = "echo deploy"
TOML
cat >"$APP/.tool-versions" <<'TOOLS'
rust 1.88.0
TOOLS
git -C "$TMP/workspace" add .
git -C "$TMP/workspace" commit -q -m init
GITHUB_OUTPUT="$TMP/resolve.out" \
GITHUB_WORKSPACE="$TMP/workspace" \
EDGEZERO_ACTION_ROOT="$ROOT" \
INPUT_WORKING_DIRECTORY=app \
INPUT_MANIFEST=edgezero.toml \
INPUT_RUST_TOOLCHAIN=auto \
INPUT_BUILD_MODE=auto \
INPUT_CACHE=true \
RUNNER_OS=Linux \
RUNNER_ARCH=X64 \
run_expect_ok resolve "$ACTION_DIR/scripts/resolve-project.sh"
grep -q '^effective-build-mode=never$' "$TMP/resolve.out" || fail "fastly auto did not resolve to never"
grep -q '^rust-toolchain=1.88.0$' "$TMP/resolve.out" || fail "toolchain discovery failed"
grep -q '^source-revision=' "$TMP/resolve.out" || fail "source revision missing"

# resolve-project can use the pinned action ref when the action root is not a Git checkout.
ACTION_NOGIT="$TMP/action-root-no-git"
mkdir -p "$ACTION_NOGIT"
cp "$ROOT/.tool-versions" "$ACTION_NOGIT/.tool-versions"
GITHUB_OUTPUT="$TMP/resolve-ref.out" \
GITHUB_WORKSPACE="$TMP/workspace" \
EDGEZERO_ACTION_ROOT="$ACTION_NOGIT" \
EDGEZERO_ACTION_REF=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
INPUT_WORKING_DIRECTORY=app \
INPUT_MANIFEST=edgezero.toml \
INPUT_RUST_TOOLCHAIN=auto \
INPUT_BUILD_MODE=auto \
INPUT_CACHE=false \
RUNNER_OS=Linux \
RUNNER_ARCH=X64 \
run_expect_ok resolve-ref "$ACTION_DIR/scripts/resolve-project.sh"
grep -q '^edgezero-revision=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa$' "$TMP/resolve-ref.out" || fail "action ref revision fallback failed"

# resolve-project rejects malformed .tool-versions instead of silently falling back.
BAD_WS="$TMP/bad-toolchain"
mkdir -p "$BAD_WS/app"
git -C "$BAD_WS" init -q
git -C "$BAD_WS" config user.email test@example.com
git -C "$BAD_WS" config user.name Test
git -C "$BAD_WS" config commit.gpgsign false
cat >"$BAD_WS/Cargo.lock" <<'LOCK'
# fixture lockfile
LOCK
cat >"$BAD_WS/app/edgezero.toml" <<'TOML'
[adapters.fastly.commands]
deploy = "echo deploy"
TOML
printf 'rust\n' >"$BAD_WS/app/.tool-versions"
git -C "$BAD_WS" add .
git -C "$BAD_WS" commit -q -m init
GITHUB_OUTPUT="$TMP/bad-toolchain.out" \
GITHUB_WORKSPACE="$BAD_WS" \
EDGEZERO_ACTION_ROOT="$ROOT" \
INPUT_WORKING_DIRECTORY=app \
INPUT_MANIFEST=edgezero.toml \
INPUT_RUST_TOOLCHAIN=auto \
INPUT_BUILD_MODE=auto \
INPUT_CACHE=false \
run_expect_fail bad-toolchain "$ACTION_DIR/scripts/resolve-project.sh"
grep -q "malformed .tool-versions rust entry" "$TMP/bad-toolchain.err" || fail "malformed toolchain rejection missing"

# resolve-project rejects dirty source.
echo dirty >>"$APP/file.txt"
GITHUB_OUTPUT="$TMP/dirty.out" \
GITHUB_WORKSPACE="$TMP/workspace" \
EDGEZERO_ACTION_ROOT="$ROOT" \
INPUT_WORKING_DIRECTORY=app \
INPUT_RUST_TOOLCHAIN=auto \
INPUT_BUILD_MODE=auto \
INPUT_CACHE=false \
run_expect_fail dirty "$ACTION_DIR/scripts/resolve-project.sh"
grep -q "working tree.*dirty" "$TMP/dirty.err" || fail "dirty tree rejection missing"
git -C "$TMP/workspace" checkout -- file.txt 2>/dev/null || rm -f "$APP/file.txt"

# run-edgezero preserves argument boundaries and keeps deploy credentials scoped.
FAKEBIN="$TMP/bin"
mkdir -p "$FAKEBIN"
cat >"$FAKEBIN/edgezero" <<'SH'
#!/usr/bin/env bash
printf '%s\0' "$@" >"$EDGEZERO_CAPTURE_ARGS"
if [[ " ${*} " == *" deploy "* ]]; then
  [[ -n "${FASTLY_API_TOKEN:-}" ]] || exit 64
  [[ -n "${FASTLY_SERVICE_ID:-}" ]] || exit 65
  [[ -z "${FASTLY_TOKEN:-}" ]] || exit 66
  [[ -z "${FASTLY_PROFILE:-}" ]] || exit 67
  [[ -z "${INPUT_FASTLY_API_TOKEN:-}" ]] || exit 68
fi
SH
chmod +x "$FAKEBIN/edgezero"
printf '%s\0' '--comment' 'hello world' >"$TMP/args.nul"
PATH="$FAKEBIN:$PATH" \
WORKING_DIRECTORY="$APP" \
ARGS_FILE="$TMP/args.nul" \
MANIFEST="$APP/edgezero.toml" \
EDGEZERO_CAPTURE_ARGS="$TMP/captured.nul" \
FASTLY_API_TOKEN=token \
FASTLY_SERVICE_ID=svc \
FASTLY_TOKEN=caller-leak \
FASTLY_PROFILE=caller-profile \
INPUT_FASTLY_API_TOKEN=caller-input-leak \
run_expect_ok run-deploy "$ACTION_DIR/scripts/run-edgezero.sh" deploy
captured=()
while IFS= read -r -d '' item; do
  captured+=("$item")
done <"$TMP/captured.nul"
expected=(deploy --adapter fastly -- --service-id svc --non-interactive --comment "hello world")
[[ ${#captured[@]} -eq ${#expected[@]} ]] || fail "captured deploy arg count mismatch"
for i in "${!expected[@]}"; do
  [[ "${captured[$i]}" == "${expected[$i]}" ]] || fail "captured deploy arg $i mismatch: expected '${expected[$i]}'"
done

# build mode must not require Fastly credentials.
PATH="$FAKEBIN:$PATH" \
WORKING_DIRECTORY="$APP" \
ARGS_FILE="$TMP/args.nul" \
MANIFEST="$APP/edgezero.toml" \
EDGEZERO_CAPTURE_ARGS="$TMP/captured-build.nul" \
run_expect_ok run-build "$ACTION_DIR/scripts/run-edgezero.sh" build

printf 'deploy action script tests passed\n'
