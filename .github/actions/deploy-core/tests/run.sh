#!/usr/bin/env bash
set -euo pipefail

# Contract tests for the EdgeZero deploy actions.
#
# Pure Bash: no Python, no network, no live provider credentials. Every test
# runs against temp dirs and fake binaries, so it is safe in CI and locally.

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd)
WORK_DIR=$(mktemp -d)
readonly REPO_ROOT WORK_DIR
readonly ACTIONS_DIR="$REPO_ROOT/.github/actions"
readonly CORE_SCRIPTS="$ACTIONS_DIR/deploy-core/scripts"
trap 'rm -rf "$WORK_DIR"' EXIT

# ---------------------------------------------------------------------------
# Tiny assertion harness
# ---------------------------------------------------------------------------
tests_passed=0
tests_failed=0

pass() {
  tests_passed=$((tests_passed + 1))
  printf '  \033[32mok\033[0m   %s\n' "$1"
}

fail() {
  tests_failed=$((tests_failed + 1))
  printf '  \033[31mFAIL\033[0m %s\n' "$1" >&2
}

# assert_succeeds "<description>" <command...>
assert_succeeds() {
  local description="$1"
  shift
  if "$@" >/dev/null 2>&1; then pass "$description"; else fail "$description"; fi
}

# assert_fails "<description>" <command...>
assert_fails() {
  local description="$1"
  shift
  if "$@" >/dev/null 2>&1; then fail "$description (expected non-zero exit)"; else pass "$description"; fi
}

# assert_equals "<description>" "<expected>" "<actual>"
assert_equals() {
  local description="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    pass "$description"
  else
    fail "$description"
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") >&2 || true
  fi
}

section() { printf '\n== %s ==\n' "$1"; }

# ---------------------------------------------------------------------------
# validate-inputs.sh — provider-neutral input validation
# ---------------------------------------------------------------------------
# Runs validate-inputs in a clean environment. Inputs are supplied by the
# caller through the VALIDATE_* variables (all optional; sane defaults below).
run_validate_inputs() {
  local state_dir
  state_dir=$(mktemp -d "$WORK_DIR/validate.XXXXXX")
  env -i PATH="$PATH" \
    EDGEZERO__ADAPTER="${VALIDATE_ADAPTER:-fastly}" \
    EDGEZERO__BUILD__CACHE="${VALIDATE_CACHE:-false}" \
    EDGEZERO__BUILD__MODE="${VALIDATE_BUILD_MODE:-auto}" \
    EDGEZERO__BUILD__ARGS="${VALIDATE_BUILD_ARGS:-[]}" \
    EDGEZERO__DEPLOY__ARGS="${VALIDATE_DEPLOY_ARGS:-[]}" \
    EDGEZERO__DEPLOY__FLAGS="${VALIDATE_DEPLOY_FLAGS:-[]}" \
    EDGEZERO__PROVIDER__ENV_CLEAR="${VALIDATE_PROVIDER_ENV_CLEAR:-[]}" \
    EDGEZERO__DEPLOY__ARG_ALLOW="${VALIDATE_ALLOW:-}" \
    EDGEZERO__DEPLOY__STAGE="${VALIDATE_STAGE:-false}" \
    EDGEZERO__ACTION__STATE_DIR="$state_dir" \
    GITHUB_OUTPUT="$state_dir/output.txt" \
    bash "$CORE_SCRIPTS/validate-inputs.sh"
}

test_validate_inputs() {
  section "validate-inputs"
  VALIDATE_ADAPTER=fastly assert_succeeds "accepts a well-formed adapter" run_validate_inputs
  VALIDATE_ADAPTER=FASTLY assert_fails "rejects a malformed adapter" run_validate_inputs
  VALIDATE_CACHE=maybe assert_fails "rejects a non-boolean cache" run_validate_inputs
  VALIDATE_STAGE=true assert_succeeds "accepts stage=true" run_validate_inputs
  VALIDATE_STAGE=True assert_fails "rejects a non-boolean stage (typo -> no silent prod)" run_validate_inputs
  VALIDATE_DEPLOY_ARGS='["--comment","hi"]' VALIDATE_ALLOW='--comment' \
    assert_succeeds "allows an allowlisted deploy-arg (--comment)" run_validate_inputs
  VALIDATE_DEPLOY_ARGS='["--service-id","x"]' VALIDATE_ALLOW='--comment' \
    assert_fails "rejects a non-allowlisted deploy-arg (--service-id)" run_validate_inputs
  VALIDATE_DEPLOY_ARGS='"not-an-array"' assert_fails "rejects non-array deploy-args" run_validate_inputs
  VALIDATE_BUILD_ARGS='[1,2]' assert_fails "rejects non-string build-args" run_validate_inputs
}

# ---------------------------------------------------------------------------
# build-app-cli artifact-name — never usable as a path traversal
# ---------------------------------------------------------------------------
check_artifact_name() {
  # Run validate_artifact_name from build-app-cli's common.sh in a subshell.
  bash -c 'source "$1"; validate_artifact_name "$2"' _ \
    "$ACTIONS_DIR/build-app-cli/scripts/common.sh" "$1"
}

test_artifact_name() {
  section "build-app-cli artifact-name"
  assert_succeeds "accepts a conservative artifact name" check_artifact_name "edgezero-cli.v1"
  assert_fails "rejects path traversal ('../x')" check_artifact_name "../x"
  assert_fails "rejects path separators ('a/b')" check_artifact_name "a/b"
  assert_fails "rejects a leading dot" check_artifact_name ".hidden"
  assert_fails "rejects an empty name" check_artifact_name ""
}

# ---------------------------------------------------------------------------
# build-app-cli reset_owned_dir — never rm -rf outside the action-owned temp root
# ---------------------------------------------------------------------------
check_owned_dir() {
  bash -c 'source "$1"; reset_owned_dir "$2" "$3"' _ \
    "$ACTIONS_DIR/build-app-cli/scripts/common.sh" "$1" "$2"
}

test_owned_dir_confinement() {
  section "build-app-cli owned-dir confinement"
  local temp_root="$WORK_DIR/temproot"
  mkdir -p "$temp_root"
  assert_succeeds "recreates a dir beneath the temp root" \
    check_owned_dir "$temp_root/build" "$temp_root"
  # An inherited value pointing at the checkout must be refused, not deleted.
  assert_fails "refuses a dir outside the temp root (would delete the checkout)" \
    check_owned_dir "$WORK_DIR/not-temp" "$temp_root"
  assert_fails "refuses a traversal path" \
    check_owned_dir "$temp_root/../escape" "$temp_root"
  # Prove the refusal did not delete anything.
  mkdir -p "$WORK_DIR/not-temp"
  check_owned_dir "$WORK_DIR/not-temp" "$temp_root" >/dev/null 2>&1 || true
  if [[ -d "$WORK_DIR/not-temp" ]]; then
    pass "the refused directory still exists (nothing was removed)"
  else
    fail "the refused directory was deleted"
  fi
}

# ---------------------------------------------------------------------------
# download-app-cli — app-cli-bin confinement + unsafe archive rejection
# ---------------------------------------------------------------------------
check_cli_bin() {
  bash -c 'source "$1"; validate_cli_bin "$2"' _ "$CORE_SCRIPTS/common.sh" "$1"
}

check_tarball() {
  bash -c 'source "$1"; assert_safe_tarball "$2"' _ "$CORE_SCRIPTS/common.sh" "$1"
}

test_cli_bin_confinement() {
  section "download-app-cli app-cli-bin + archive safety"
  assert_succeeds "accepts a bare app-cli-bin" check_cli_bin "myapp-cli"
  assert_fails "rejects a traversal app-cli-bin ('../../outside/tool')" check_cli_bin "../../outside/tool"
  assert_fails "rejects an app-cli-bin with a separator" check_cli_bin "sub/tool"
  assert_fails "rejects an empty app-cli-bin" check_cli_bin ""

  # A tar carrying a symlink member must be refused before extraction.
  local evil="$WORK_DIR/evil"
  mkdir -p "$evil/stage"
  ln -sf /etc/passwd "$evil/stage/pwned"
  tar -C "$evil/stage" -cf "$evil/evil.tar" pwned 2>/dev/null
  assert_fails "refuses an archive containing a symlink member" check_tarball "$evil/evil.tar"

  # A well-formed archive is accepted.
  local good="$WORK_DIR/good"
  mkdir -p "$good/stage"
  echo x >"$good/stage/myapp-cli"
  printf '{}' >"$good/stage/app-cli-meta.json"
  tar -C "$good/stage" -cf "$good/good.tar" myapp-cli app-cli-meta.json
  assert_succeeds "accepts a well-formed archive" check_tarball "$good/good.tar"
}

# ---------------------------------------------------------------------------
# run-app-cli.sh — provider-env credential boundary
# ---------------------------------------------------------------------------
# A fake CLI records the FASTLY_* it actually saw; run-cli must clear inherited
# aliases and export only the declared, typed values.
test_provider_env_boundary() {
  section "run-cli provider-env boundary"

  local bin_dir="$WORK_DIR/pe-bin" app_dir="$WORK_DIR/pe-app"
  local seen="$WORK_DIR/pe-seen.txt" clear="$WORK_DIR/pe-clear.nul"
  mkdir -p "$bin_dir" "$app_dir"
  cat >"$bin_dir/fakecli" <<EOF
#!/usr/bin/env bash
{
  printf 'TOKEN=%s\n' "\${FASTLY_API_TOKEN-unset}"
  printf 'ENDPOINT=%s\n' "\${FASTLY_ENDPOINT-unset}"
} >"$seen"
EOF
  chmod +x "$bin_dir/fakecli"
  printf 'FASTLY_API_TOKEN\0FASTLY_ENDPOINT\0' >"$clear"

  run_deploy_pe() {
    env -i PATH="$bin_dir:$PATH" \
      EDGEZERO__APP__CLI__BIN=fakecli EDGEZERO__ADAPTER=fastly \
      EDGEZERO__PROJECT__WORKING_DIRECTORY="$app_dir" \
      EDGEZERO__PROVIDER__ENV_CLEAR_FILE="$clear" \
      EDGEZERO__PROVIDER__ENV="$1" \
      FASTLY_API_TOKEN=inherited-BAD FASTLY_ENDPOINT=https://inherited.invalid \
      bash "$CORE_SCRIPTS/run-app-cli.sh" deploy
  }

  if run_deploy_pe '{"FASTLY_API_TOKEN":"typed-tok"}' >/dev/null 2>&1; then
    assert_equals "typed token wins; inherited endpoint cleared" \
      $'TOKEN=typed-tok\nENDPOINT=unset' "$(cat "$seen")"
  else
    fail "run-cli deploy (provider-env) failed to execute"
  fi

  # A provider-env name not declared in provider-env-clear is rejected.
  assert_fails "rejects an undeclared provider-env name" \
    run_deploy_pe '{"FASTLY_TOKEN":"x"}'
}

# ---------------------------------------------------------------------------
# run-app-cli.sh — CLI argv construction
# ---------------------------------------------------------------------------
# Installs a fake CLI that records its argv, then asserts run-cli places typed
# deploy-flags before `--` and caller passthrough after `--`.
test_run_cli_argv() {
  section "run-cli argv"

  local bin_dir="$WORK_DIR/bin"
  local argv_file="$WORK_DIR/recorded-argv.txt"
  local app_dir="$WORK_DIR/app"
  mkdir -p "$bin_dir" "$app_dir"

  cat >"$bin_dir/fakecli" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$@" >"$argv_file"
EOF
  chmod +x "$bin_dir/fakecli"

  # NUL-delimited argument files, exactly as validate-inputs would emit them.
  printf -- '--service-id\0abc\0--stage\0' >"$WORK_DIR/deploy-flags.nul"
  printf -- '--comment\0hello\0' >"$WORK_DIR/deploy-args.nul"

  if env -i PATH="$bin_dir:$PATH" \
    EDGEZERO__APP__CLI__BIN=fakecli \
    EDGEZERO__ADAPTER=fastly \
    EDGEZERO__PROJECT__WORKING_DIRECTORY="$app_dir" \
    EDGEZERO__DEPLOY__FLAGS_FILE="$WORK_DIR/deploy-flags.nul" \
    EDGEZERO__DEPLOY__ARGS_FILE="$WORK_DIR/deploy-args.nul" \
    bash "$CORE_SCRIPTS/run-app-cli.sh" deploy >/dev/null 2>&1; then
    local expected
    expected=$'deploy\n--adapter\nfastly\n--service-id\nabc\n--stage\n--\n--comment\nhello'
    assert_equals "flags precede --, passthrough follows --" "$expected" "$(cat "$argv_file")"
  else
    fail "run-cli deploy failed to execute"
  fi
}

# ---------------------------------------------------------------------------
# download-app-cli.sh — self-describing artifact
# ---------------------------------------------------------------------------
# Builds a fake artifact tar (binary + app-cli-meta.json) and asserts download-cli
# extracts it and surfaces the metadata.
test_download_cli_metadata() {
  section "download-app-cli metadata"

  local artifact_dir="$WORK_DIR/artifact"
  local stage_dir="$artifact_dir/stage"
  mkdir -p "$stage_dir"

  cat >"$stage_dir/myapp-cli" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod +x "$stage_dir/myapp-cli"
  printf '{"app-cli-bin":"myapp-cli","app-cli-version":"1.2.3","app-cli-package":"myapp-cli"}\n' \
    >"$stage_dir/app-cli-meta.json"
  tar -C "$stage_dir" -cf "$artifact_dir/edgezero-cli.tar" myapp-cli app-cli-meta.json

  local output_file="$WORK_DIR/download-output.txt"
  if env -i PATH="$PATH" \
    EDGEZERO__APP__CLI__ARTIFACT_DIR="$artifact_dir" \
    EDGEZERO__ACTION__TOOL_ROOT="$WORK_DIR/tools" \
    GITHUB_OUTPUT="$output_file" \
    GITHUB_PATH="$WORK_DIR/download-path.txt" \
    bash "$CORE_SCRIPTS/download-app-cli.sh" >/dev/null 2>&1; then
    if grep -qx 'app-cli-bin=myapp-cli' "$output_file" && grep -qx 'app-cli-version=1.2.3' "$output_file"; then
      pass "extracts the tar and reads app-cli-meta.json"
    else
      fail "download-app-cli did not surface the expected metadata"
    fi
  else
    fail "download-app-cli failed to execute"
  fi
}

# ---------------------------------------------------------------------------
# versions.json — pinned Fastly CLI metadata
# ---------------------------------------------------------------------------
# The pinned Fastly version must agree with .tool-versions and the checksum
# must be a well-formed SHA-256 (replaces the old Python metadata check).
check_fastly_versions() {
  command -v jq >/dev/null 2>&1 || return 0
  local versions_json="$ACTIONS_DIR/deploy-fastly/versions.json"
  local pinned tool_versions_entry checksum
  pinned=$(jq -er '.fastly.version' "$versions_json")
  tool_versions_entry=$(awk '$1 == "fastly" { print $2 }' "$REPO_ROOT/.tool-versions")
  [[ "$pinned" == "$tool_versions_entry" ]] || return 1
  checksum=$(jq -er '.fastly.linux_amd64.sha256' "$versions_json")
  [[ ${#checksum} -eq 64 && "$checksum" =~ ^[0-9a-f]+$ ]]
}

test_fastly_versions() {
  section "Fastly versions.json"
  assert_succeeds "pinned version matches .tool-versions and sha256 is well-formed" check_fastly_versions
}

# ---------------------------------------------------------------------------
# cleanup.sh — it runs `rm -rf`, so confinement is the whole contract
# ---------------------------------------------------------------------------
test_cleanup_confinement() {
  section "cleanup confinement"
  local temp_root="$WORK_DIR/cleanup-temp" outside="$WORK_DIR/cleanup-outside"
  mkdir -p "$temp_root/owned" "$outside/checkout"

  RUNNER_TEMP="$temp_root" EDGEZERO__ACTION__TOOL_ROOT="$temp_root/owned" \
    EDGEZERO__ACTION__STATE_DIR="" "$CORE_SCRIPTS/cleanup.sh" >/dev/null 2>&1 || true
  assert_fails "removes an action-owned dir beneath RUNNER_TEMP" test -d "$temp_root/owned"

  # The original defect: cleanup removed $EDGEZERO_FASTLY_HOME, a variable the
  # action never set — so its value could only ever be inherited. Any dir handed
  # to cleanup from outside the temp root must be refused, not deleted.
  RUNNER_TEMP="$temp_root" EDGEZERO__ACTION__TOOL_ROOT="$outside/checkout" \
    EDGEZERO__ACTION__STATE_DIR="" "$CORE_SCRIPTS/cleanup.sh" >/dev/null 2>&1 || true
  assert_succeeds "refuses a dir outside RUNNER_TEMP (the checkout survives)" test -d "$outside/checkout"

  # A symlink must not smuggle the removal out of the temp root either.
  ln -s "$outside/checkout" "$temp_root/link-out"
  RUNNER_TEMP="$temp_root" EDGEZERO__ACTION__TOOL_ROOT="$temp_root/link-out" \
    EDGEZERO__ACTION__STATE_DIR="" "$CORE_SCRIPTS/cleanup.sh" >/dev/null 2>&1 || true
  assert_succeeds "refuses a symlink pointing outside RUNNER_TEMP" test -d "$outside/checkout"

  RUNNER_TEMP="" EDGEZERO__ACTION__TOOL_ROOT="$outside/checkout" \
    assert_succeeds "no RUNNER_TEMP: removes nothing" "$CORE_SCRIPTS/cleanup.sh"
}

# ---------------------------------------------------------------------------
# run-app-cli.sh — the action's private env must not survive into the app CLI
# ---------------------------------------------------------------------------
test_action_env_scrub() {
  section "action-private env scrub"
  local dir="$WORK_DIR/scrub"
  mkdir -p "$dir/bin"
  # A stand-in CLI that reports the environment it was handed.
  cat >"$dir/bin/scrub-cli" <<'CLI'
#!/usr/bin/env bash
printf 'FASTLY_API_TOKEN=%s\n' "${FASTLY_API_TOKEN:-ABSENT}"
printf 'EDGEZERO__PROVIDER__ENV=%s\n' "${EDGEZERO__PROVIDER__ENV:-ABSENT}"
printf 'EDGEZERO__FASTLY__API_TOKEN=%s\n' "${EDGEZERO__FASTLY__API_TOKEN:-ABSENT}"
printf 'EDGEZERO__DEPLOY__ARGS_FILE=%s\n' "${EDGEZERO__DEPLOY__ARGS_FILE:-ABSENT}"
printf 'EDGEZERO_MANIFEST=%s\n' "${EDGEZERO_MANIFEST:-ABSENT}"
CLI
  chmod +x "$dir/bin/scrub-cli"
  printf 'FASTLY_API_TOKEN\0' >"$dir/clear.nul"

  local out
  out=$(
    PATH="$dir/bin:$PATH" \
      EDGEZERO__APP__CLI__BIN=scrub-cli EDGEZERO__ADAPTER=fastly EDGEZERO__PROJECT__WORKING_DIRECTORY="$dir" \
      EDGEZERO__PROJECT__MANIFEST_PATH="$dir/edgezero.toml" \
      EDGEZERO__PROVIDER__ENV_CLEAR_FILE="$dir/clear.nul" \
      EDGEZERO__PROVIDER__ENV='{"FASTLY_API_TOKEN":"s3cret"}' \
      EDGEZERO__FASTLY__API_TOKEN='s3cret' \
      "$CORE_SCRIPTS/run-app-cli.sh" deploy 2>/dev/null
  )

  # What the CLI IS promised.
  assert_equals "the typed provider alias is delivered" \
    "FASTLY_API_TOKEN=s3cret" "$(grep '^FASTLY_API_TOKEN=' <<<"$out")"
  assert_equals "EDGEZERO_MANIFEST is delivered" \
    "EDGEZERO_MANIFEST=$dir/edgezero.toml" "$(grep '^EDGEZERO_MANIFEST=' <<<"$out")"

  # What it must NEVER see: the same secret under names we never promised.
  assert_equals "the provider-env JSON blob does not survive" \
    "EDGEZERO__PROVIDER__ENV=ABSENT" "$(grep '^EDGEZERO__PROVIDER__ENV=' <<<"$out")"
  assert_equals "the action's token carrier does not survive" \
    "EDGEZERO__FASTLY__API_TOKEN=ABSENT" "$(grep '^EDGEZERO__FASTLY__API_TOKEN=' <<<"$out")"
  assert_equals "action-private file handles do not survive" \
    "EDGEZERO__DEPLOY__ARGS_FILE=ABSENT" "$(grep '^EDGEZERO__DEPLOY__ARGS_FILE=' <<<"$out")"
}

# ---------------------------------------------------------------------------
# validate-inputs.sh — action-owned passthrough bypasses the caller allowlist
# ---------------------------------------------------------------------------
test_deploy_args_prepend() {
  section "action-owned deploy-args prepend"
  local state="$WORK_DIR/prepend"
  local out args
  out=$(
    EDGEZERO__ACTION__STATE_DIR="$state" EDGEZERO__ADAPTER=fastly \
      EDGEZERO__DEPLOY__ARG_ALLOW="--comment" \
      EDGEZERO__DEPLOY__ARGS='["--comment","hi"]' \
      EDGEZERO__DEPLOY__ARGS_PREPEND='["--non-interactive"]' \
      "$CORE_SCRIPTS/validate-inputs.sh"
  )
  args=$(tr '\0' '\n' <"$state/deploy-args.nul")
  # `--non-interactive` is action-owned: it is NOT caller input, so it is not
  # allowlist-checked, and it must come first.
  assert_equals "action-owned args are prepended, caller args preserved" \
    $'--non-interactive\n--comment\nhi' "$args"
  [[ -n "$out" ]] || true

  # A caller still cannot smuggle it in themselves.
  assert_fails "the caller allowlist still rejects --non-interactive from deploy-args" \
    env EDGEZERO__ACTION__STATE_DIR="$state" EDGEZERO__ADAPTER=fastly \
    EDGEZERO__DEPLOY__ARG_ALLOW="--comment" \
    EDGEZERO__DEPLOY__ARGS='["--non-interactive"]' \
    "$CORE_SCRIPTS/validate-inputs.sh"
}

# ---------------------------------------------------------------------------
# common.sh — anchored version parsing, required inputs, private logs
# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# run-app-cli.sh — provider values must survive the Bash boundary intact
# ---------------------------------------------------------------------------
# `export NAME=value` truncates at the first NUL, so a NUL-bearing credential
# would be silently altered rather than rejected. The guard must reject NUL and
# still accept ordinary values — a NUL check that also rejects spaces would break
# every real token.
test_provider_env_nul() {
  section "provider-env NUL rejection"
  local dir="$WORK_DIR/nul"
  mkdir -p "$dir/bin" "$dir/app"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$dir/bin/nul-cli"
  chmod +x "$dir/bin/nul-cli"
  printf 'FASTLY_API_TOKEN\0' >"$dir/clear.nul"

  run_with_env() {
    PATH="$dir/bin:$PATH" \
      EDGEZERO__APP__CLI__BIN=nul-cli EDGEZERO__ADAPTER=fastly \
      EDGEZERO__PROJECT__WORKING_DIRECTORY="$dir/app" \
      EDGEZERO__PROVIDER__ENV_CLEAR_FILE="$dir/clear.nul" \
      EDGEZERO__PROVIDER__ENV="$1" \
      "$CORE_SCRIPTS/run-app-cli.sh" deploy >/dev/null 2>&1
  }

  # jq builds the NUL: a raw NUL cannot survive argv, which is the whole point.
  local nul_json
  nul_json=$(jq -nc '{FASTLY_API_TOKEN: "abc\u0000def"}')
  assert_fails "a NUL-bearing provider value is rejected" run_with_env "$nul_json"

  # A NUL check must not become a space check.
  assert_succeeds "an ordinary value containing spaces is accepted" \
    run_with_env '{"FASTLY_API_TOKEN":"tok with spaces"}'
  assert_succeeds "a plain token is accepted" \
    run_with_env '{"FASTLY_API_TOKEN":"abc123"}'
}

test_lifecycle_helpers() {
  section "lifecycle helpers"
  # NB: sourced in subshells only — common.sh defines its own `fail`, which would
  # otherwise clobber this harness's.
  local helpers="source '$CORE_SCRIPTS/common.sh'"

  local log="$WORK_DIR/version.log"
  # An UNanchored parser reads `version=15.2.0` as 15 and `version=12abc` as 12,
  # threading a version that was never deployed into healthcheck and rollback.
  printf 'version=15.2.0\nversion=12abc\n' >"$log"
  assert_equals "a malformed version line yields nothing (never a prefix guess)" \
    "" "$(bash -c "$helpers; read_numeric_line version '$log'")"
  printf 'noise\nversion=41\nversion=42\n' >"$log"
  assert_equals "the last well-formed version line wins" \
    "42" "$(bash -c "$helpers; read_numeric_line version '$log'")"
  printf 'healthy=maybe\n' >"$log"
  assert_equals "a non-boolean verdict yields nothing" \
    "" "$(bash -c "$helpers; read_bool_line healthy '$log'")"

  # GitHub Actions does not enforce `required: true`, so these are the real guard.
  assert_fails "an empty required input is rejected" \
    bash -c "source '$CORE_SCRIPTS/common.sh'; require_input fastly-service-id ''"
  assert_fails "a required input that fails its pattern is rejected" \
    bash -c "source '$CORE_SCRIPTS/common.sh'; require_input_matching fastly-version '15.2.0' '^[0-9]+\$'"
  assert_succeeds "a well-formed required input is accepted" \
    bash -c "source '$CORE_SCRIPTS/common.sh'; require_input_matching fastly-version '42' '^[0-9]+\$'"

  # `fail` always exits 1, which erases a provider CLI's real status. Wrappers
  # use fail_with so an operator's retry/branch logic sees the true code.
  local rc=0
  bash -c "$helpers; fail_with 3 'boom'" >/dev/null 2>&1 || rc=$?
  assert_equals "fail_with preserves the tool's exit status" "3" "$rc"
  rc=0
  bash -c "$helpers; fail_with 0 'boom'" >/dev/null 2>&1 || rc=$?
  assert_equals "fail_with never turns a failure into success (0 -> 1)" "1" "$rc"
  rc=0
  bash -c "$helpers; fail_with '' 'boom'" >/dev/null 2>&1 || rc=$?
  assert_equals "fail_with rejects a blank status (-> 1)" "1" "$rc"

  # Provider CLIs print request URLs and service metadata; the log must not be
  # left behind in RUNNER_TEMP for later steps in the job to read.
  local leaked
  leaked=$(
    RUNNER_TEMP="$WORK_DIR" bash -c "
      source '$CORE_SCRIPTS/common.sh'
      new_private_log
      printf '%s\n' \"\$LIFECYCLE_LOG\"
    "
  )
  assert_fails "the private log is removed when its owner exits" test -e "$leaked"
}

# ---------------------------------------------------------------------------
# build-app-cli.sh — the toolchain search must not cross the app's Git boundary
# ---------------------------------------------------------------------------
test_toolchain_boundary() {
  section "toolchain search boundary"
  # The adoption guide's layout: a deployer repo at github.workspace, with the
  # application checked out into a subdirectory. The DEPLOYER's .tool-versions
  # must never decide which Rust compiles the APPLICATION.
  local ws="$WORK_DIR/tc-workspace"
  mkdir -p "$ws/app"
  printf 'rust 1.60.0\n' >"$ws/.tool-versions"
  git -C "$ws/app" init -q 2>/dev/null || return 0
  printf 'rust 1.95.0\n' >"$ws/app/.tool-versions"

  local resolved
  resolved=$(
    bash -c "
      source '$ACTIONS_DIR/build-app-cli/scripts/build-app-cli.sh'
      resolve_rust_toolchain auto '$ws/app' '$ws' '$REPO_ROOT'
    "
  )
  assert_equals "the app's own .tool-versions wins over the deployer's" "1.95.0" "$resolved"

  # With no toolchain file in the app repo, the search must STOP at the app's
  # Git root rather than picking up the deployer's file one level up.
  rm -f "$ws/app/.tool-versions"
  local fallback
  fallback=$(
    bash -c "
      source '$ACTIONS_DIR/build-app-cli/scripts/build-app-cli.sh'
      resolve_rust_toolchain auto '$ws/app' '$ws' '$REPO_ROOT'
    "
  )
  local edgezero_rust
  edgezero_rust=$(awk '$1 == "rust" { print $2 }' "$REPO_ROOT/.tool-versions")
  assert_equals "the search stops at the app's Git root (deployer's 1.60.0 ignored)" \
    "$edgezero_rust" "$fallback"
}

# ---------------------------------------------------------------------------
# config-push.sh — the staging key is a different key, driven by --staging
# ---------------------------------------------------------------------------
# Runs config-push.sh against a fake app CLI that records its argv and emits the
# canonical pushed-key line. Returns the recorded argv (one arg per line).
run_config_push_argv() {
  local dir="$WORK_DIR/config-push"
  rm -rf "$dir"
  mkdir -p "$dir/bin" "$dir/app"
  # A fake app CLI: record every argument, then emit the contract line so the
  # wrapper's anchored parse succeeds.
  cat >"$dir/bin/fake-cli" <<'CLI'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$FAKE_ARGV_OUT"
echo "pushed-key=app_config_staging"
echo "pushed-store=app_config"
CLI
  chmod +x "$dir/bin/fake-cli"
  # An in-app file every call can reference (this helper recreates $dir, so the
  # fixture must live here rather than being made by the caller).
  printf 'x\n' >"$dir/app/real.toml"

  PATH="$dir/bin:$PATH" FAKE_ARGV_OUT="$dir/argv.txt" \
    EDGEZERO__APP__CLI__BIN=fake-cli \
    FASTLY_API_TOKEN=tok \
    GITHUB_WORKSPACE="$dir" \
    EDGEZERO__PROJECT__WORKING_DIRECTORY=app \
    EDGEZERO__DEPLOY__TO="${CP_DEPLOY_TO:-production}" \
    EDGEZERO__CONFIG_PUSH__STORE="${CP_STORE:-}" \
    EDGEZERO__CONFIG_PUSH__KEY="${CP_KEY:-}" \
    EDGEZERO__CONFIG_PUSH__MANIFEST="${CP_MANIFEST:-}" \
    EDGEZERO__CONFIG_PUSH__APP_CONFIG="${CP_APP_CONFIG:-}" \
    "$ACTIONS_DIR/config-push-fastly/scripts/config-push.sh" >/dev/null 2>&1
  cat "$dir/argv.txt" 2>/dev/null
}

# Run config-push.sh with a caller-supplied path; used for confinement checks.
config_push_rejects_path() {
  local var="$1" value="$2"
  local dir="$WORK_DIR/config-push"
  env "$var=$value" PATH="$dir/bin:$PATH" FAKE_ARGV_OUT="$dir/argv.txt" \
    EDGEZERO__APP__CLI__BIN=fake-cli FASTLY_API_TOKEN=tok \
    GITHUB_WORKSPACE="$dir" EDGEZERO__PROJECT__WORKING_DIRECTORY=app \
    "$ACTIONS_DIR/config-push-fastly/scripts/config-push.sh"
}

test_config_push_argv() {
  section "config-push argv"

  # Production: the base subcommand, non-interactive flags, and NO --staging.
  local prod
  prod=$(run_config_push_argv)
  assert_equals "production drives 'config push --adapter fastly'" \
    $'config\npush\n--adapter\nfastly\n--yes\n--no-diff' "$prod"

  # Staging: same argv plus --staging (the CLI then writes <key>_staging).
  local staged
  staged=$(CP_DEPLOY_TO=staging run_config_push_argv)
  assert_succeeds "staging appends --staging" grep -qx -- '--staging' <<<"$staged"
  assert_fails "production does NOT pass --staging" grep -qx -- '--staging' <<<"$prod"

  # Typed --store / --key are threaded through when supplied.
  local with_store
  with_store=$(CP_STORE=cfg CP_KEY=mykey run_config_push_argv)
  assert_succeeds "--store is threaded" grep -qx -- 'cfg' <<<"$with_store"
  assert_succeeds "--key is threaded" grep -qx -- 'mykey' <<<"$with_store"

  # A bad deploy-to must fail closed, never silently push to production.
  assert_fails "a non-{production,staging} deploy-to is rejected" \
    env EDGEZERO__APP__CLI__BIN=fake-cli FASTLY_API_TOKEN=tok \
    GITHUB_WORKSPACE="$WORK_DIR/config-push" EDGEZERO__PROJECT__WORKING_DIRECTORY=app \
    EDGEZERO__DEPLOY__TO=Staging \
    "$ACTIONS_DIR/config-push-fastly/scripts/config-push.sh"

  # Path confinement: manifest/app-config are caller strings handed to a
  # credential-bearing CLI, so nothing may escape the app directory.
  local dir="$WORK_DIR/config-push"
  printf 'secret\n' >"$WORK_DIR/outside.toml"
  ln -sf "$WORK_DIR/outside.toml" "$dir/app/escape.toml"

  assert_fails "an absolute manifest path is rejected" \
    config_push_rejects_path EDGEZERO__CONFIG_PUSH__MANIFEST "$WORK_DIR/outside.toml"
  assert_fails "a traversal manifest path is rejected" \
    config_push_rejects_path EDGEZERO__CONFIG_PUSH__MANIFEST "../outside.toml"
  assert_fails "a symlink escaping the app dir is rejected" \
    config_push_rejects_path EDGEZERO__CONFIG_PUSH__MANIFEST "escape.toml"
  assert_fails "an absolute app-config path is rejected" \
    config_push_rejects_path EDGEZERO__CONFIG_PUSH__APP_CONFIG "$WORK_DIR/outside.toml"

  # Confinement must not over-reject: an in-app path still works.
  local ok
  ok=$(CP_MANIFEST=real.toml run_config_push_argv || true)
  assert_succeeds "an in-app manifest path is accepted and threaded" \
    grep -qx -- 'real.toml' <<<"$ok"
}

# ---------------------------------------------------------------------------
# run-app-cli.sh — the CLI's exit status is the step's exit status
# ---------------------------------------------------------------------------
# A deploy that fails must fail the step. If the engine swallowed the exit code,
# a broken deploy would report success and the caller would never roll back.
test_exit_propagation() {
  section "exit propagation"
  local dir="$WORK_DIR/exit-prop"
  mkdir -p "$dir/bin" "$dir/app"
  cat >"$dir/bin/exit-cli" <<'CLI'
#!/usr/bin/env bash
exit "${FAKE_EXIT_CODE:-0}"
CLI
  chmod +x "$dir/bin/exit-cli"

  run_with_exit() {
    PATH="$dir/bin:$PATH" FAKE_EXIT_CODE="$1" \
      EDGEZERO__APP__CLI__BIN=exit-cli EDGEZERO__ADAPTER=fastly \
      EDGEZERO__PROJECT__WORKING_DIRECTORY="$dir/app" \
      "$CORE_SCRIPTS/run-app-cli.sh" build >/dev/null 2>&1
  }

  # NB: capture with `|| rc=$?` — a trailing `|| true` would reset $? to 0 and
  # make this test vacuously pass.
  local rc=0
  run_with_exit 0 || rc=$?
  assert_equals "a succeeding CLI exits 0" "0" "$rc"
  rc=0
  run_with_exit 42 || rc=$?
  assert_equals "a failing CLI's exit code reaches the step (42, not 1)" "42" "$rc"
}

# ---------------------------------------------------------------------------
# resolve-project.sh — deploys require committed source
# ---------------------------------------------------------------------------
# The dirty-source guard is what makes `source-revision` honest: it is the
# revision that was DEPLOYED, so an uncommitted edit must not ship under a clean
# SHA. Modified, staged, and untracked all count as dirty.
test_dirty_source_guard() {
  section "dirty-source guard"
  local repo="$WORK_DIR/dirty-src"
  mkdir -p "$repo"
  git -C "$repo" init -q 2>/dev/null || return 0
  git -C "$repo" config user.email t@t.invalid
  git -C "$repo" config user.name t
  echo one >"$repo/file.txt"
  git -C "$repo" add -A && git -C "$repo" commit -qm init

  # resolve-project.sh guards its own main(), so sourcing it just exposes the
  # guard function (no project resolution, no cargo).
  local guard="source '$CORE_SCRIPTS/resolve-project.sh'"

  assert_succeeds "a clean tree passes" \
    bash -c "$guard; assert_committed_source '$repo' app"

  echo two >>"$repo/file.txt"
  assert_fails "an unstaged modification is dirty" \
    bash -c "$guard; assert_committed_source '$repo' app"

  git -C "$repo" add -A
  assert_fails "a staged-but-uncommitted change is dirty" \
    bash -c "$guard; assert_committed_source '$repo' app"

  git -C "$repo" commit -qm two
  echo x >"$repo/untracked.txt"
  assert_fails "an untracked file is dirty (it would ship unbuilt)" \
    bash -c "$guard; assert_committed_source '$repo' app"
}

# ---------------------------------------------------------------------------
# resolve-project.sh — the cache key is exact
# ---------------------------------------------------------------------------
# The cache key decides whether a build reuses target/. If it omits an input that
# changes the artifacts, CI silently ships a stale build. Cargo.lock is only
# hashed (never parsed), so a minimal fixture proves the composition offline.
cache_key_for() {
  local ws="$WORK_DIR/cache-key"
  # NB: the output file lives OUTSIDE the fixture repo — inside it, it would be
  # an untracked file and the dirty-source guard would (correctly) reject it.
  local out="$WORK_DIR/cache-key-out.txt"
  : >"$out"
  env -i PATH="$PATH" HOME="${HOME:-/tmp}" \
    GITHUB_WORKSPACE="$ws" \
    GITHUB_OUTPUT="$out" \
    RUNNER_OS=Linux RUNNER_ARCH=X64 \
    EDGEZERO__ACTION__ROOT="$REPO_ROOT" \
    EDGEZERO__PROJECT__WORKING_DIRECTORY=app \
    EDGEZERO__PROJECT__RUST_TOOLCHAIN="${CK_TOOLCHAIN:-1.95.0}" \
    EDGEZERO__PROJECT__TARGET="${CK_TARGET:-wasm32-wasip1}" \
    EDGEZERO__APP__CLI__VERSION="${CK_CLI_VERSION:-1.0.0}" \
    EDGEZERO__BUILD__CACHE="${CK_CACHE:-false}" \
    bash "$CORE_SCRIPTS/resolve-project.sh" >/dev/null 2>&1 || return $?
  grep -oE '^cache-key=.*$' "$out" | tail -n 1 | cut -d= -f2-
}

test_cache_key() {
  section "cache key"
  local ws="$WORK_DIR/cache-key"
  mkdir -p "$ws/app/src"
  cat >"$ws/app/Cargo.toml" <<'TOML'
[package]
name = "ck-fixture"
version = "0.1.0"
edition = "2021"
TOML
  echo 'fn main() {}' >"$ws/app/src/main.rs"
  printf 'version = 3\n' >"$ws/app/Cargo.lock"
  git -C "$ws" init -q 2>/dev/null || return 0
  git -C "$ws" config user.email t@t.invalid
  git -C "$ws" config user.name t
  git -C "$ws" add -A && git -C "$ws" commit -qm init

  local base
  base=$(cache_key_for) || { fail "resolve-project could not produce a cache key"; return 0; }
  [[ -n "$base" ]] || { fail "cache key is empty"; return 0; }

  assert_succeeds "the key is namespaced and carries OS+arch" \
    grep -qE '^edgezero-deploy-Linux-X64-' <<<"$base"

  # Each input that changes the artifacts must change the key.
  assert_fails "a different toolchain changes the key" \
    bash -c "[[ '$(CK_TOOLCHAIN=1.60.0 cache_key_for)' == '$base' ]]"
  assert_fails "a different target changes the key" \
    bash -c "[[ '$(CK_TARGET=wasm32-unknown-unknown cache_key_for)' == '$base' ]]"
  assert_fails "a different app-CLI version changes the key" \
    bash -c "[[ '$(CK_CLI_VERSION=2.0.0 cache_key_for)' == '$base' ]]"

  # The lockfile hash is the point: new deps must not reuse an old target/.
  printf 'version = 3\n# changed\n' >"$ws/app/Cargo.lock"
  git -C "$ws" add -A && git -C "$ws" commit -qm lockfile-change
  assert_fails "a changed Cargo.lock busts the key (no stale target/ reuse)" \
    bash -c "[[ '$(cache_key_for)' == '$base' ]]"

  # cache: true with no lockfile cannot key exactly — fail rather than guess.
  rm -f "$ws/app/Cargo.lock"
  git -C "$ws" add -A && git -C "$ws" commit -qm drop-lockfile
  if CK_CACHE=true cache_key_for >/dev/null 2>&1; then
    fail "cache=true without Cargo.lock was accepted (cannot key exactly)"
  else
    pass "cache=true without Cargo.lock is rejected"
  fi
}

# ---------------------------------------------------------------------------
# action.yml metadata — every declared output is produced by the step it names
# ---------------------------------------------------------------------------
# A declared output whose step never emits that name silently resolves to "".
# That is exactly how the app-cli-artifact rename broke the deploy wiring: the
# consumers read an output the producer no longer wrote.
#
# This resolves each `steps.<id>.outputs.<name>` to the SPECIFIC script that step
# runs, so a name emitted by some other action cannot vouch for this one. Both
# `outputs['name']` and `outputs.name` spellings are recognised — GitHub accepts
# either, so a test that only understood one would silently skip the rest.

# Echo "<step-id> <script-path>" for every step in an action.yml that runs a
# script, resolving $GITHUB_ACTION_PATH to the action's own directory.
action_step_scripts() {
  local action="$1" action_dir
  action_dir=$(dirname "$action")
  awk -v dir="$action_dir" '
    /^[[:space:]]*-[[:space:]]*name:/ { id = "" }
    /^[[:space:]]*id:[[:space:]]*/    { id = $2 }
    /^[[:space:]]*run:.*\.sh/ {
      if (id == "") next
      line = $0
      sub(/^[[:space:]]*run:[[:space:]]*/, "", line)
      gsub(/\$GITHUB_ACTION_PATH/, dir, line)
      gsub(/\$\{\{[^}]*\}\}/, "", line)
      print id, line
      id = ""
    }
  ' "$action"
}

# ---------------------------------------------------------------------------
# action.yml metadata — public surface is well-formed
# ---------------------------------------------------------------------------
# Pure Bash/awk (no Python, per the project's tooling rule). actionlint only
# parses composite metadata it reaches through a `uses:`, and these wrappers are
# also consumed directly by callers — so check every action.yml on its own.
#
# The duplicate-env-key case is not hypothetical: a bad edit on this branch
# defined the same key twice in one step, which YAML resolves silently to the
# last value.
# ---------------------------------------------------------------------------
# resolve-project.sh — the app REPOSITORY is the boundary, not github.workspace
# ---------------------------------------------------------------------------
# In the separate-repository layout the deployer repo IS github.workspace, so
# "inside the workspace" is not a boundary at all: a `../deployer/edgezero.toml`
# manifest, or a Cargo workspace root that `cargo locate-project` climbs into,
# would build code that `source-revision` never describes.

# Build a deployer-repo-at-the-workspace-root layout with the app checked out
# beneath it as its OWN repository. $1 is the workspace dir; when $2 is
# "capture", the deployer's Cargo workspace lists the app as a member — which is
# how `cargo locate-project --workspace` climbs out of the app repository.
make_boundary_fixture() {
  local ws="$1" mode="${2:-independent}"
  mkdir -p "$ws/app/src" "$ws/deployer"
  printf 'name = "deployer-manifest"\n' >"$ws/deployer/edgezero.toml"

  if [[ "$mode" == "capture" ]]; then
    printf '[workspace]\nmembers = ["app"]\nresolver = "2"\n' >"$ws/Cargo.toml"
    # A member of the parent workspace: no [workspace] of its own.
    printf '[package]\nname = "bnd-fixture"\nversion = "0.1.0"\nedition = "2021"\n' >"$ws/app/Cargo.toml"
  else
    # Its own workspace root, so cargo stops inside the app repository.
    printf '[package]\nname = "bnd-fixture"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n' >"$ws/app/Cargo.toml"
  fi

  echo 'fn main() {}' >"$ws/app/src/main.rs"
  printf 'version = 3\n' >"$ws/app/Cargo.lock"
  printf 'name = "app-manifest"\n' >"$ws/app/edgezero.toml"

  git -C "$ws" init -q
  git -C "$ws" config user.email t@t.invalid
  git -C "$ws" config user.name t
  git -C "$ws" add -A && git -C "$ws" commit -qm deployer
  git -C "$ws/app" init -q
  git -C "$ws/app" config user.email t@t.invalid
  git -C "$ws/app" config user.name t
  git -C "$ws/app" add -A && git -C "$ws/app" commit -qm app
}

run_resolve_in() {
  local ws="$1"
  env -i PATH="$PATH" HOME="${HOME:-/tmp}" \
    GITHUB_WORKSPACE="$ws" GITHUB_OUTPUT="$WORK_DIR/boundary-out.txt" \
    RUNNER_OS=Linux RUNNER_ARCH=X64 \
    EDGEZERO__ACTION__ROOT="$REPO_ROOT" \
    EDGEZERO__PROJECT__WORKING_DIRECTORY=app \
    EDGEZERO__PROJECT__RUST_TOOLCHAIN=1.95.0 \
    EDGEZERO__PROJECT__TARGET=wasm32-wasip1 \
    EDGEZERO__PROJECT__MANIFEST="${BND_MANIFEST:-}" \
    bash "$CORE_SCRIPTS/resolve-project.sh"
}

test_app_repo_boundary() {
  section "app repository boundary"
  local ok="$WORK_DIR/bnd-ok"
  mkdir -p "$ok"
  git -C "$ok" init -q 2>/dev/null || return 0
  rm -rf "$ok"
  mkdir -p "$ok"
  make_boundary_fixture "$ok" independent

  # The boundary must not over-reject a legitimate app.
  assert_succeeds "an app that owns its workspace resolves" run_resolve_in "$ok"
  BND_MANIFEST=edgezero.toml \
    assert_succeeds "the app's own manifest is accepted" run_resolve_in "$ok"

  # Inside github.workspace, but a different repository than source-revision names.
  BND_MANIFEST=../deployer/edgezero.toml \
    assert_fails "a manifest in the deployer repo is rejected" run_resolve_in "$ok"

  # The deployer's workspace claims the app, so cargo resolves the workspace root
  # OUT of the app repository — we would build and cache the deployer's tree.
  local cap="$WORK_DIR/bnd-capture"
  mkdir -p "$cap"
  make_boundary_fixture "$cap" capture
  assert_fails "a Cargo workspace root outside the app repository is rejected" \
    run_resolve_in "$cap"
}

test_action_metadata() {
  section "action metadata"
  local action bad=0

  for action in "$ACTIONS_DIR"/*/action.yml; do
    local who; who=$(basename "$(dirname "$action")")

    # Required top-level keys.
    local key
    for key in name description runs; do
      grep -qE "^${key}:" "$action" ||
        { fail "$who action.yml has no top-level '$key:'"; bad=$((bad + 1)); }
    done

    # Every declared input needs a description — it is the public contract.
    local undescribed
    undescribed=$(awk '
      /^inputs:/ { in_inputs = 1; next }
      /^[a-z]+:/ && !/^inputs:/ { in_inputs = 0 }
      in_inputs && /^  [a-z][a-z0-9-]*:/ {
        if (name != "" && !described) print name
        name = $1; sub(/:$/, "", name); described = 0
      }
      in_inputs && /^    description:/ { described = 1 }
      END { if (name != "" && !described) print name }
    ' "$action")
    if [[ -n "$undescribed" ]]; then
      fail "$who has inputs without a description: $(tr '\n' ' ' <<<"$undescribed")"
      bad=$((bad + 1))
    fi

    # A key defined twice in ONE step's env: YAML keeps the last silently.
    local dupes
    dupes=$(awk '
      /^    - name:/ { delete seen; next }
      /^      env:/ { in_env = 1; next }
      /^      [a-z]+:/ { in_env = 0 }
      in_env && /^        [A-Za-z_][A-Za-z0-9_]*:/ {
        k = $1; sub(/:$/, "", k)
        if (k in seen) print k
        seen[k] = 1
      }
    ' "$action" | sort -u)
    if [[ -n "$dupes" ]]; then
      fail "$who defines the same env key twice in one step: $(tr '\n' ' ' <<<"$dupes")"
      bad=$((bad + 1))
    fi
  done

  [[ "$bad" -eq 0 ]] && pass "every action.yml declares a well-formed public surface"
}

test_action_output_contracts() {
  section "action output contracts"
  local action missing=0 checked=0

  for action in "$ACTIONS_DIR"/*/action.yml; do
    local name_of; name_of=$(basename "$(dirname "$action")")
    local scripts; scripts=$(action_step_scripts "$action")

    local ref step_id out_name script emitted
    # Both spellings: steps.<id>.outputs['<name>'] and steps.<id>.outputs.<name>
    while IFS= read -r ref; do
      [[ -n "$ref" ]] || continue
      step_id=${ref%% *}
      out_name=${ref##* }
      checked=$((checked + 1))

      script=$(awk -v want="$step_id" '$1 == want { $1 = ""; sub(/^ /, ""); print; exit }' <<<"$scripts")
      if [[ -z "$script" ]]; then
        fail "$name_of output '$out_name' names step '$step_id', which runs no script"
        missing=$((missing + 1))
        continue
      fi
      if [[ ! -f "$script" ]]; then
        fail "$name_of step '$step_id' points at a missing script: $script"
        missing=$((missing + 1))
        continue
      fi
      # The named step's OWN script must emit it — not merely some other action.
      emitted=$(grep -oE "append_output ${out_name}( |\$)" "$script" || true)
      if [[ -z "$emitted" ]]; then
        fail "$name_of output '$out_name' claims step '$step_id' ($(basename "$script")) emits it, but that script does not"
        missing=$((missing + 1))
      fi
    done < <(sed -n '/^outputs:/,/^runs:/p' "$action" |
      grep -oE "steps\.[a-z-]+\.outputs(\['[a-z0-9-]+'\]|\.[a-z0-9-]+)" |
      sed -E "s/steps\.([a-z-]+)\.outputs\['([a-z0-9-]+)'\]/\1 \2/; s/steps\.([a-z-]+)\.outputs\.([a-z0-9-]+)/\1 \2/" |
      sort -u)
  done

  if [[ "$checked" -eq 0 ]]; then
    fail "the output-contract test matched no outputs at all (it is not testing anything)"
  elif [[ "$missing" -eq 0 ]]; then
    pass "all $checked declared outputs are emitted by the step they name"
  fi
}

# ---------------------------------------------------------------------------
main() {
  test_validate_inputs
  test_artifact_name
  test_owned_dir_confinement
  test_cli_bin_confinement
  test_run_cli_argv
  test_provider_env_boundary
  test_download_cli_metadata
  test_fastly_versions
  test_cleanup_confinement
  test_action_env_scrub
  test_deploy_args_prepend
  test_provider_env_nul
  test_lifecycle_helpers
  test_toolchain_boundary
  test_config_push_argv
  test_exit_propagation
  test_dirty_source_guard
  test_cache_key
  test_app_repo_boundary
  test_action_metadata
  test_action_output_contracts

  printf '\nPassed: %d  Failed: %d\n' "$tests_passed" "$tests_failed"
  [[ "$tests_failed" -eq 0 ]]
}

main "$@"
