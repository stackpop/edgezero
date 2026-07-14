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
    EDGEZERO__INPUT__ADAPTER="${VALIDATE_ADAPTER:-fastly}" \
    EDGEZERO__INPUT__CACHE="${VALIDATE_CACHE:-false}" \
    EDGEZERO__INPUT__BUILD_MODE="${VALIDATE_BUILD_MODE:-auto}" \
    EDGEZERO__INPUT__BUILD_ARGS="${VALIDATE_BUILD_ARGS:-[]}" \
    EDGEZERO__INPUT__DEPLOY_ARGS="${VALIDATE_DEPLOY_ARGS:-[]}" \
    EDGEZERO__INPUT__DEPLOY_FLAGS="${VALIDATE_DEPLOY_FLAGS:-[]}" \
    EDGEZERO__INPUT__PROVIDER_ENV_CLEAR="${VALIDATE_PROVIDER_ENV_CLEAR:-[]}" \
    EDGEZERO__INPUT__DEPLOY_ARG_ALLOW="${VALIDATE_ALLOW:-}" \
    EDGEZERO__INPUT__STAGE="${VALIDATE_STAGE:-false}" \
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
    EDGEZERO__ACTION__STATE_DIR="$state" EDGEZERO__INPUT__ADAPTER=fastly \
      EDGEZERO__INPUT__DEPLOY_ARG_ALLOW="--comment" \
      EDGEZERO__INPUT__DEPLOY_ARGS='["--comment","hi"]' \
      EDGEZERO__INPUT__DEPLOY_ARGS_PREPEND='["--non-interactive"]' \
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
    env EDGEZERO__ACTION__STATE_DIR="$state" EDGEZERO__INPUT__ADAPTER=fastly \
    EDGEZERO__INPUT__DEPLOY_ARG_ALLOW="--comment" \
    EDGEZERO__INPUT__DEPLOY_ARGS='["--non-interactive"]' \
    "$CORE_SCRIPTS/validate-inputs.sh"
}

# ---------------------------------------------------------------------------
# common.sh — anchored version parsing, required inputs, private logs
# ---------------------------------------------------------------------------
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
  test_lifecycle_helpers
  test_toolchain_boundary

  printf '\nPassed: %d  Failed: %d\n' "$tests_passed" "$tests_failed"
  [[ "$tests_failed" -eq 0 ]]
}

main "$@"
