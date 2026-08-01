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

# assert_fails_with "<description>" "<expected-stderr-substring>" <command...>
# Asserts the command fails AND fails for the expected REASON. A bare exit-code
# check can pass by accident when the command would have failed later anyway
# (e.g. a missing required variable AFTER the scrub check we meant to exercise).
assert_fails_with() {
  local description="$1" needle="$2"
  shift 2
  local out
  if out=$("$@" 2>&1); then
    fail "$description (expected non-zero exit)"
  elif [[ "$out" == *"$needle"* ]]; then
    pass "$description"
  else
    fail "$description (failed, but not with: $needle)"
  fi
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
  # download-app-cli.sh checks require_linux_x86_64 (it runs the Linux artifact's
  # --help), so main() only completes on a Linux x86-64 runner. CI's static-checks
  # job is Linux and exercises it for real.
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) ;;
    *)
      pass "download-app-cli metadata (skipped: non-Linux runner)"
      return
      ;;
  esac

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
    # The ABSOLUTE path output is what lets callers dodge PATH shadowing.
    if grep -qx "app-cli-path=$WORK_DIR/tools/bin/myapp-cli" "$output_file"; then
      pass "emits the absolute app-cli-path"
    else
      fail "download-app-cli did not emit app-cli-path"
    fi
    # It must NOT prepend the app-bin dir to PATH: an app CLI legitimately named
    # after a tool the action shells out to (e.g. `jq`) would otherwise shadow it.
    if [[ ! -s "$WORK_DIR/download-path.txt" ]] || ! grep -q '/tools/bin' "$WORK_DIR/download-path.txt"; then
      pass "does not prepend the app-bin dir to PATH"
    else
      fail "download-app-cli prepended the app-bin dir to GITHUB_PATH (can shadow jq etc.)"
    fi
  else
    fail "download-app-cli failed to execute"
  fi
}

# ---------------------------------------------------------------------------
# wrapper validate.sh — the per-wrapper input validation (now scripts, not inline
# YAML, so it is shellcheck'd AND testable). GitHub does not enforce
# `required: true`, so these guards are the real gate.
# ---------------------------------------------------------------------------
test_wrapper_validate() {
  section "wrapper validate.sh"

  # deploy-fastly: artifact + token presence, service-id format, then it delegates
  # to the real engine validate-inputs.sh — so the success case runs end to end
  # (the engine needs a supported runner + adapter).
  local dfl="$ACTIONS_DIR/deploy-fastly/scripts/validate.sh"
  run_dfl() {
    env EDGEZERO__APP__CLI__ARTIFACT_PRESENT="${A:-true}" \
      EDGEZERO__FASTLY__API_TOKEN_PRESENT="${T:-true}" \
      EDGEZERO__FASTLY__SERVICE_ID="${S-svc_1}" \
      EDGEZERO__ADAPTER=fastly EDGEZERO__RUNNER__OS=Linux EDGEZERO__RUNNER__ARCH=X64 \
      EDGEZERO__ACTION__STATE_DIR="$WORK_DIR/dfl-state" \
      GITHUB_OUTPUT="$WORK_DIR/dfl-out.txt" \
      bash "$dfl"
  }
  assert_succeeds "deploy-fastly: well-formed inputs pass" run_dfl
  A=false assert_fails "deploy-fastly: missing artifact is rejected" run_dfl
  T=false assert_fails "deploy-fastly: missing token (by presence) is rejected" run_dfl
  S='bad id!' assert_fails "deploy-fastly: malformed service-id is rejected" run_dfl
  S='' assert_fails "deploy-fastly: empty service-id is rejected" run_dfl

  # config-push-fastly: artifact + token presence, deploy-to fail-closed.
  local cpf="$ACTIONS_DIR/config-push-fastly/scripts/validate.sh"
  run_cpf() {
    env EDGEZERO__APP__CLI__ARTIFACT_PRESENT="${A:-true}" \
      EDGEZERO__FASTLY__API_TOKEN_PRESENT="${T:-true}" \
      EDGEZERO__DEPLOY__TO="${D:-production}" \
      EDGEZERO__CONFIG_PUSH__KEY_PRESENT="${K:-false}" bash "$cpf"
  }
  assert_succeeds "config-push: production passes" run_cpf
  D=staging assert_succeeds "config-push: staging passes" run_cpf
  D=Staging assert_fails "config-push: a deploy-to typo is rejected (no silent prod)" run_cpf
  A=false assert_fails "config-push: missing artifact is rejected" run_cpf
  # A staging key is derived, so an explicit key with staging is refused early.
  D=production K=true assert_succeeds "config-push: an explicit key is fine for production" run_cpf
  D=staging K=true assert_fails "config-push: key + staging is rejected up front" run_cpf

  # healthcheck + rollback: artifact presence only.
  local hc="$ACTIONS_DIR/healthcheck-fastly/scripts/validate.sh"
  assert_succeeds "healthcheck: present artifact passes" \
    env EDGEZERO__APP__CLI__ARTIFACT_PRESENT=true bash "$hc"
  assert_fails "healthcheck: missing artifact is rejected" \
    env EDGEZERO__APP__CLI__ARTIFACT_PRESENT=false bash "$hc"
  local rb="$ACTIONS_DIR/rollback-fastly/scripts/validate.sh"
  assert_fails "rollback: missing artifact is rejected" \
    env EDGEZERO__APP__CLI__ARTIFACT_PRESENT=false bash "$rb"
}

# ---------------------------------------------------------------------------
# resolve_app_cli — invoke the absolute path, so a `fastly`-named app CLI is not
# shadowed by the provider CLI the install step prepends to PATH.
# ---------------------------------------------------------------------------
test_resolve_app_cli() {
  section "app-cli resolution (PATH shadowing)"
  local resolved
  resolved=$(EDGEZERO__APP__CLI__PATH=/opt/tools/bin/fastly EDGEZERO__APP__CLI__BIN=fastly \
    bash -c "source '$CORE_SCRIPTS/common.sh'; resolve_app_cli")
  assert_equals "prefers the absolute path when set" "/opt/tools/bin/fastly" "$resolved"

  resolved=$(EDGEZERO__APP__CLI__BIN=myapp-cli \
    bash -c "source '$CORE_SCRIPTS/common.sh'; resolve_app_cli")
  assert_equals "falls back to the bare name when no path is given" "myapp-cli" "$resolved"

  assert_fails "fails when neither is set" \
    bash -c "source '$CORE_SCRIPTS/common.sh'; resolve_app_cli"
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
# Print the lines of the step whose `- ` header contains "$2", from that header
# up to (but not including) the next top-level step.
step_block() {
  awk -v want="$2" '
    /^    - / { inb = (index($0, want) > 0) }
    inb { print }
  ' "$1"
}

test_workspace_step_scrub() {
  section "workspace steps scrub credentials"
  # The prepare/cleanup steps run before validation, so — like every other step —
  # they MUST blank the shipped aliases and BASH_ENV. Otherwise an inherited token
  # plus a checkout-controlled BASH_ENV runs code with the token before the body.
  local a p block
  for a in build-app-cli deploy-fastly healthcheck-fastly rollback-fastly config-push-fastly; do
    p="$ACTIONS_DIR/$a/action.yml"
    block=$(step_block "$p" "Prepare action workspace")
    assert_succeeds "$a: prepare step blanks FASTLY_API_TOKEN" grep -qF 'FASTLY_API_TOKEN: ""' <<<"$block"
    assert_succeeds "$a: prepare step blanks BASH_ENV" grep -qF 'BASH_ENV: ""' <<<"$block"
  done
  # Every build-app-cli run: bash step must blank BASH_ENV — including the publish
  # step, which holds the REAL GITHUB_OUTPUT and is the handoff-validation boundary,
  # and the cleanup step.
  local step
  for step in "Compile application CLI package" "Publish CLI outputs" "Cleanup workspace"; do
    block=$(step_block "$ACTIONS_DIR/build-app-cli/action.yml" "$step")
    assert_succeeds "build-app-cli: '$step' blanks FASTLY_API_TOKEN" grep -qF 'FASTLY_API_TOKEN: ""' <<<"$block"
    assert_succeeds "build-app-cli: '$step' blanks BASH_ENV" grep -qF 'BASH_ENV: ""' <<<"$block"
  done
}

test_workspace_isolation() {
  section "per-invocation workspace isolation"
  # Each action must mint a UNIQUE per-invocation root (mktemp -d) so two
  # concurrent invocations in one job never share fixed temp paths — otherwise one
  # could overwrite the other's CLI/state/flags and run them with the wrong token.
  local a p
  for a in build-app-cli deploy-fastly healthcheck-fastly rollback-fastly config-push-fastly; do
    p="$ACTIONS_DIR/$a/action.yml"
    assert_succeeds "$a: mints its workspace via the shared prepare-workspace.sh" \
      grep -qF 'deploy-core/scripts/prepare-workspace.sh' "$p"
    assert_fails "$a: leaves no fixed runner.temp/edgezero path" \
      grep -qE 'runner\.temp \}\}/edgezero-' "$p"
    assert_succeeds "$a: cleanup removes the workspace root" \
      grep -qF 'EDGEZERO__ACTION__WORKSPACE:' "$p"
  done

  # prepare-workspace.sh mints a validated mktemp -d root under RUNNER_TEMP. The
  # root is validated SEPARATELY (a masked mktemp failure would publish an empty
  # root, escaping RUNNER_TEMP).
  local prep="$CORE_SCRIPTS/prepare-workspace.sh"
  assert_succeeds "prepare-workspace mints a mktemp -d root" grep -qF 'mktemp -d' "$prep"
  assert_succeeds "prepare-workspace validates the root before publishing" \
    grep -qF 'could not create the action workspace' "$prep"
  local prt="$WORK_DIR/prep-rt" pout="$WORK_DIR/prep-out" minted
  rm -rf "$prt"
  mkdir -p "$prt"
  : >"$pout"
  RUNNER_TEMP="$prt" GITHUB_OUTPUT="$pout" bash "$prep"
  minted=$(sed -n 's/^root=//p' "$pout")
  assert_succeeds "prepare-workspace publishes a real dir under RUNNER_TEMP" \
    bash -c "[ -d '$minted' ] && case '$minted' in '$prt'/*) exit 0;; *) exit 1;; esac"

  # cleanup.sh actually removes EDGEZERO__ACTION__WORKSPACE (confined to RUNNER_TEMP).
  local rt="$WORK_DIR/ws-clean" ws
  rm -rf "$rt"
  mkdir -p "$rt"
  ws=$(mktemp -d "$rt/edgezero.XXXXXX")
  touch "$ws/file"
  RUNNER_TEMP="$rt" EDGEZERO__ACTION__WORKSPACE="$ws" bash "$CORE_SCRIPTS/cleanup.sh" >/dev/null 2>&1
  assert_fails "cleanup removes the per-invocation workspace" test -d "$ws"
  # ...but refuses a workspace OUTSIDE RUNNER_TEMP (same confinement as the rest).
  local outside="$WORK_DIR/ws-outside"
  rm -rf "$outside"
  mkdir -p "$outside"
  RUNNER_TEMP="$rt" EDGEZERO__ACTION__WORKSPACE="$outside" bash "$CORE_SCRIPTS/cleanup.sh" >/dev/null 2>&1
  assert_succeeds "cleanup refuses a workspace outside RUNNER_TEMP" test -d "$outside"
}

test_no_inline_action_scripts() {
  section "actions use script files, never inline run:"
  # Every composite `run:` must invoke a `.sh` file — no inline shell in YAML, which
  # would be neither shellcheck'd nor testable. A `run: |` block header (or any
  # `run:` line without a `.sh`) fails this.
  local p bad
  for p in "$ACTIONS_DIR"/*/action.yml; do
    bad=$(grep -nE '^[[:space:]]*run:' "$p" | grep -vF '.sh' || true)
    assert_equals "$(basename "$(dirname "$p")"): every run: invokes a .sh script" "" "$bad"
  done
}

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
# ---------------------------------------------------------------------------
# clear_provider_env_aliases — build-app-cli runs APP-CONTROLLED code (cargo
# build, the built CLI's --help), so every caller-named provider credential must
# be unset first. The names come from the input, so the helper is provider-neutral.
# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# versions.json must pin an OFFICIAL release. make-fake-fastly-env.sh repoints
# this file at a local fake archive (that is how the smoke exercises the real
# download+verify path), so running it locally dirties a tracked file. If such an
# override were ever committed, the real installer would try to fetch from a
# machine-local path — this guard fails fast instead.
# ---------------------------------------------------------------------------
test_versions_json_pins_official_release() {
  section "versions.json pins an official release"
  local vj="$ACTIONS_DIR/deploy-fastly/versions.json"
  local url verdict
  url=$(jq -r '.fastly.linux_amd64.url' "$vj")
  case "$url" in
    https://github.com/fastly/cli/releases/download/*) verdict=official ;;
    *) verdict="$url" ;;
  esac
  assert_equals "versions.json pins an official https release URL (never a local file:// override)" \
    "official" "$verdict"

  # The URL must point at the pinned VERSION: a version bump that forgets to
  # update the URL — or an URL swapped to a different build — is a real regression
  # the checksum alone would not localize.
  local version expected_tail
  version=$(jq -r '.fastly.version' "$vj")
  expected_tail="/releases/download/v${version}/fastly_v${version}_linux-amd64.tar.gz"
  case "$url" in
    *"$expected_tail") verdict=matches ;;
    *) verdict="$url" ;;
  esac
  assert_equals "versions.json URL filename embeds the pinned version" "matches" "$verdict"

  local sha
  sha=$(jq -r '.fastly.linux_amd64.sha256' "$vj")
  case "$sha" in
    [0-9a-f]*) verdict=$([ ${#sha} -eq 64 ] && echo sha256 || echo "$sha") ;;
    *) verdict="$sha" ;;
  esac
  assert_equals "versions.json pins a 64-hex SHA-256" "sha256" "$verdict"
}

test_clear_provider_env_aliases() {
  section "provider-env-clear scrubbing"
  local lib="$ACTIONS_DIR/build-app-cli/scripts/common.sh"

  # --- input validation must FAIL CLOSED -------------------------------------
  # A permissive parse (`jq '.[]?'`) accepts these and yields NO names, silently
  # leaving every inherited credential in scope. Each must be an error instead.
  local bad
  for bad in '"FASTLY_API_TOKEN"' '{}' 'null' '123' 'true' '"[]"'; do
    assert_fails "valid-but-not-an-array input is rejected: $bad" \
      bash -c "source '$lib'; provider_env_clear_names '$bad'"
  done
  assert_fails "invalid JSON is rejected" \
    bash -c "source '$lib'; provider_env_clear_names 'not-json'"
  assert_fails "a non-string member is rejected" \
    bash -c "source '$lib'; provider_env_clear_names '[\"OK\",42]'"
  assert_fails "an empty-string member is rejected" \
    bash -c "source '$lib'; provider_env_clear_names '[\"OK\",\"\"]'"
  assert_fails "an invalid environment variable name is rejected" \
    bash -c "source '$lib'; provider_env_clear_names '[\"not a name\"]'"
  assert_succeeds "an empty array is a no-op" \
    bash -c "source '$lib'; provider_env_clear_names '[]'"

  local names
  names=$(bash -c "source '$lib'; provider_env_clear_names '[\"A_TOKEN\",\"B_TOKEN\"]' | tr '\n' ' '")
  assert_equals "a well-formed array yields its names" "A_TOKEN B_TOKEN " "$names"

  # A JSON member with an ESCAPED control character (valid JSON, decoding to a
  # newline/NUL) must be REJECTED. Otherwise it would reach `jq -r`, split into
  # two "names" (newline) or be truncated (NUL), leaving the real variable
  # untouched. Fixtures are printf'd so this source carries no raw control chars.
  local bs=$'\\'
  printf '["A%snB"]' "$bs" >"$WORK_DIR/pec-nl.json"
  printf '["WRONG%su0000SECRET"]' "$bs" >"$WORK_DIR/pec-nul.json"
  assert_fails "an escaped newline in a name is rejected" \
    bash -c "source '$lib'; provider_env_clear_names \"\$(cat '$WORK_DIR/pec-nl.json')\""
  assert_fails "an escaped NUL in a name is rejected" \
    bash -c "source '$lib'; provider_env_clear_names \"\$(cat '$WORK_DIR/pec-nul.json')\""

  # --- the scrub must clear the credential from ANCESTOR /proc, not only $$ ----
  # A child spawned after the scrub must not find the credential in its parent's
  # (`/proc/<ppid>/environ`) environment. The re-exec (`env -u` + exec) gives the
  # script process a clean environ, so the child's parent (the script) is clean.
  # This mirrors build-app-cli.sh's arg-SENTINEL guard (an env sentinel would be
  # forgeable via job env).
  local sentinel="--edgezero-provider-env-cleared"
  local probe="$WORK_DIR/scrub-probe.sh"
  cat >"$probe" <<PROBE
#!/usr/bin/env bash
set -euo pipefail
source '$lib'
if [[ "\${1:-}" == "$sentinel" ]]; then
  shift
else
  exec_with_cleared_provider_env "\${PROBE_CLEAR:-[]}" "\$0" "$sentinel" "\$@"
fi
child() {
  local own="\${SECRET_TOKEN-unset}"
  local anc="n/a" ghenv="n/a" ghout="n/a"
  if [[ -r "/proc/\$PPID/environ" ]]; then
    local env_dump
    env_dump=\$(tr '\0' '\n' <"/proc/\$PPID/environ")
    if grep -q '^SECRET_TOKEN=' <<<"\$env_dump"; then anc="leaked"; else anc="clean"; fi
    # The GitHub file-command channels are NOT provider credentials and are never
    # named in provider-env-clear, yet their real paths must also be gone from the
    # ancestor: otherwise a build script recovers one, derives the sibling
    # GITHUB_ENV, and appends LD_PRELOAD to the real file for a later step.
    if grep -q '^GITHUB_ENV=' <<<"\$env_dump"; then ghenv="leaked"; else ghenv="clean"; fi
    if grep -q '^GITHUB_OUTPUT=' <<<"\$env_dump"; then ghout="leaked"; else ghout="clean"; fi
  fi
  printf 'own=[%s] anc=[%s] ghenv=[%s] ghout=[%s]' "\$own" "\$anc" "\$ghenv" "\$ghout"
}
export -f child
bash -c child
PROBE
  chmod +x "$probe"

  # GITHUB_ENV / GITHUB_OUTPUT are set in the environment but deliberately NOT in
  # PROBE_CLEAR — only the unconditional file-command strip can remove them.
  local scrubbed
  scrubbed=$(SECRET_TOKEN=super-secret GITHUB_ENV="$WORK_DIR/fake-github-env" \
    GITHUB_OUTPUT="$WORK_DIR/fake-github-output" \
    PROBE_CLEAR='["SECRET_TOKEN"]' bash "$probe")
  case "$scrubbed" in
    'own=[unset] anc=[clean] ghenv=[clean] ghout=[clean]' | 'own=[unset] anc=[n/a] ghenv=[n/a] ghout=[n/a]')
      assert_equals "no credential or GitHub file-command path leaks from the scrubbed ancestor" "ok" "ok" ;;
    *)
      assert_equals "no credential or GitHub file-command path leaks from the scrubbed ancestor" \
        "own=[unset] anc=[clean|n/a] ghenv=[clean|n/a] ghout=[clean|n/a]" "$scrubbed" ;;
  esac

  # An unrelated variable must survive the re-exec.
  local kept
  kept=$(SECRET_TOKEN=s KEEP_ME=kept bash -c "
    source '$lib'
    exec_with_cleared_provider_env '[\"SECRET_TOKEN\"]' /bin/bash -c 'printf \"%s\" \"\${KEEP_ME-unset}\"'")
  assert_equals "an unrelated variable survives the scrub" "kept" "$kept"

  # --- fail-closed propagation -----------------------------------------------
  # A malformed input must abort the CALLER, not just the validation subshell:
  # otherwise the caller would exec with an EMPTY strip list, running app code
  # with the credentials intact.
  local leaked
  leaked=$(SECRET_TOKEN=super-secret bash -c "source '$lib'
      exec_with_cleared_provider_env '{}' /bin/bash -c 'printf \"%s\" \"\${SECRET_TOKEN-unset}\"'" 2>/dev/null || true)
  assert_equals "a malformed input never reaches the command with credentials intact" "" "$leaked"

  # An inherited env sentinel must NOT bypass the scrub: the guard keys off an
  # ARGUMENT, so the legacy env var is inert and a malformed input still fails.
  # Assert the SCRUB VALIDATION diagnostic, not merely a non-zero exit: the script
  # would also exit later on the unset EDGEZERO__ACTION__ROOT, so a bare exit-code
  # check would pass even if the sentinel HAD bypassed validation.
  assert_fails_with "an inherited env sentinel does not bypass validation" \
    "input 'provider-env-clear' must be a JSON array" \
    env EDGEZERO__PROVIDER__ENV_CLEARED=1 EDGEZERO__PROVIDER__ENV_CLEAR='{}' \
    GITHUB_WORKSPACE="$WORK_DIR" \
    bash "$ACTIONS_DIR/build-app-cli/scripts/build-app-cli.sh"

  # The real build script must also refuse to proceed on a malformed input, and
  # do so AT THE SCRUB — before it reaches any app-controlled command.
  assert_fails_with "build-app-cli.sh fails closed on a malformed provider-env-clear" \
    "input 'provider-env-clear' must be a JSON array" \
    env EDGEZERO__PROVIDER__ENV_CLEAR='{}' GITHUB_WORKSPACE="$WORK_DIR" \
    bash "$ACTIONS_DIR/build-app-cli/scripts/build-app-cli.sh"

  # The action must `exec` the script from its run body, so no dirty wrapper-shell
  # ancestor survives for app code to walk up to. Guard against silently dropping it.
  grep -qE 'run: exec .*build-app-cli\.sh' "$ACTIONS_DIR/build-app-cli/action.yml" ||
    fail "build-app-cli action must 'exec' the build script (eliminates the dirty wrapper-shell ancestor)"

  # BASH_ENV / ENV are sourced at shell STARTUP, before the script's re-exec scrub
  # runs, so the scrub cannot clear them — they must be blanked statically in the
  # build step's `env:`. Guard against a regression that drops the static clear.
  local action_yml="$ACTIONS_DIR/build-app-cli/action.yml"
  grep -qE '^[[:space:]]+BASH_ENV: ""' "$action_yml" ||
    fail "build-app-cli build step must statically blank BASH_ENV (sourced before the scrub can run)"
  grep -qE '^[[:space:]]+ENV: ""' "$action_yml" ||
    fail "build-app-cli build step must statically blank ENV (sourced before the scrub can run)"

  # `run_untrusted` must point the GitHub Actions file-command channels away from
  # the real per-step files: an app-controlled build script could otherwise append
  # (e.g.) LD_PRELOAD to $GITHUB_ENV and have a LATER step run it with a credential
  # in scope, or forge this action's outputs via $GITHUB_OUTPUT.
  local ru_env ru_out
  ru_env="$WORK_DIR/ru-github-env"
  ru_out="$WORK_DIR/ru-github-out"
  : >"$ru_env"
  : >"$ru_out"
  GITHUB_ENV="$ru_env" GITHUB_OUTPUT="$ru_out" bash -c "
    source '$lib'
    run_untrusted /bin/bash -c 'printf \"LD_PRELOAD=/evil.so\n\" >>\"\$GITHUB_ENV\"; printf \"forged=1\n\" >>\"\$GITHUB_OUTPUT\"'
    append_output real value
  " >/dev/null 2>&1
  assert_equals "run_untrusted discards a child's GITHUB_ENV writes" "" "$(cat "$ru_env")"
  # The parent's own append_output still reaches the real file; the child's forged
  # output does not.
  assert_equals "the parent still writes real outputs while the child's are discarded" \
    "real=value" "$(cat "$ru_out")"
}

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

  # An extensionless `rust-toolchain` in TOML form must resolve its channel, not
  # the literal `[toolchain]` header line. rustup accepts both forms. The fixture
  # uses the `stable` channel keyword — distinct from the `[toolchain]` header
  # (which a broken parser would return), so a pass proves the file was parsed as
  # TOML. It names a channel, not a pinned version.
  printf '[toolchain]\nchannel = "stable"\n' >"$ws/app/rust-toolchain"
  local toml_form
  toml_form=$(
    bash -c "
      source '$ACTIONS_DIR/build-app-cli/scripts/build-app-cli.sh'
      resolve_rust_toolchain auto '$ws/app' '$ws' '$REPO_ROOT'
    "
  )
  assert_equals "a TOML-form extensionless rust-toolchain resolves its channel" "stable" "$toml_form"
  # The same file must resolve identically through resolve-project.sh's copy.
  local toml_form_deploy
  toml_form_deploy=$(
    bash -c "
      source '$ACTIONS_DIR/deploy-core/scripts/resolve-project.sh'
      parse_toolchain_from_channel_file '$ws/app/rust-toolchain'
    "
  )
  assert_equals "resolve-project.sh parses the TOML-form channel too" "stable" "$toml_form_deploy"
  rm -f "$ws/app/rust-toolchain"

  # A trailing `# comment` after the channel value is valid TOML and must parse.
  printf '[toolchain]\nchannel = "stable" # pinned\n' >"$ws/app/rust-toolchain.toml"
  local commented
  commented=$(
    bash -c "
      source '$ACTIONS_DIR/build-app-cli/scripts/build-app-cli.sh'
      resolve_rust_toolchain auto '$ws/app' '$ws' '$REPO_ROOT'
    "
  )
  assert_equals "a channel with a trailing comment parses" "stable" "$commented"
  rm -f "$ws/app/rust-toolchain.toml"
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
# Capture the --app-config file's content while it still exists (the wrapper
# removes an inline temp file on exit), so a test can verify what was pushed.
prev=""
for a in "$@"; do
  if [[ "$prev" == "--app-config" ]]; then cp -f "$a" "$FAKE_ARGV_OUT.appconfig" 2>/dev/null || true; fi
  prev="$a"
done
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
    EDGEZERO__CONFIG_PUSH__APP_CONFIG_INLINE="${CP_APP_CONFIG_INLINE:-}" \
    EDGEZERO__CONFIG_PUSH__NO_ENV="${CP_NO_ENV:-false}" \
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

  # Inline config: threaded as --app-config pointing at an action-owned temp file
  # that holds exactly the supplied content (no checkout file required).
  local inline_argv
  inline_argv=$(CP_APP_CONFIG_INLINE='greeting = "hi"' run_config_push_argv)
  assert_succeeds "inline config threads --app-config" grep -qx -- '--app-config' <<<"$inline_argv"
  assert_equals "inline content is written to the file the CLI reads" \
    'greeting = "hi"' "$(cat "$WORK_DIR/config-push/argv.txt.appconfig" 2>/dev/null)"

  # A key the wrapper does not own (contains '/') must be ACCEPTED, not rejected
  # after the CLI already wrote it — Fastly constrains key length, not this charset.
  local cpdir="$WORK_DIR/config-push"
  printf '#!/usr/bin/env bash\necho "pushed-key=release/canary"\necho "pushed-store=app_config"\n' >"$cpdir/bin/fake-cli"
  chmod +x "$cpdir/bin/fake-cli"
  : >"$cpdir/ghout"
  env PATH="$cpdir/bin:$PATH" EDGEZERO__APP__CLI__BIN=fake-cli FASTLY_API_TOKEN=tok \
    GITHUB_WORKSPACE="$cpdir" EDGEZERO__PROJECT__WORKING_DIRECTORY=app \
    RUNNER_TEMP="$cpdir" GITHUB_OUTPUT="$cpdir/ghout" \
    "$ACTIONS_DIR/config-push-fastly/scripts/config-push.sh" >/dev/null 2>&1
  assert_succeeds "a pushed key containing '/' is accepted, not rejected post-write" \
    grep -qx 'pushed-key=release/canary' "$cpdir/ghout"

  # cd into the app dir must precede the mutation-attempted signal: a directory-
  # entry failure means the CLI was never invoked, so it must not falsely signal.
  local cp="$ACTIONS_DIR/config-push-fastly/scripts/config-push.sh" cd_line sig_line
  cd_line=$(grep -n 'could not enter working-directory' "$cp" | head -1 | cut -d: -f1)
  sig_line=$(grep -n 'append_output mutation-attempted' "$cp" | head -1 | cut -d: -f1)
  assert_succeeds "config-push cd precedes the mutation-attempted signal" \
    test "$cd_line" -lt "$sig_line"

  # no-env: --no-env is appended only when requested.
  local noenv_argv
  noenv_argv=$(CP_NO_ENV=true run_config_push_argv)
  assert_succeeds "no-env=true appends --no-env" grep -qx -- '--no-env' <<<"$noenv_argv"
  assert_fails "the default does NOT pass --no-env" grep -qx -- '--no-env' <<<"$prod"

  # A file path and inline content are mutually exclusive, and no-env must be a
  # boolean — both fail closed with a named diagnostic (never a silent default).
  assert_fails_with "app-config and app-config-inline are mutually exclusive" \
    "mutually exclusive" \
    env PATH="$WORK_DIR/config-push/bin:$PATH" EDGEZERO__APP__CLI__BIN=fake-cli FASTLY_API_TOKEN=tok \
    GITHUB_WORKSPACE="$WORK_DIR/config-push" EDGEZERO__PROJECT__WORKING_DIRECTORY=app \
    EDGEZERO__CONFIG_PUSH__APP_CONFIG=real.toml EDGEZERO__CONFIG_PUSH__APP_CONFIG_INLINE='x = 1' \
    "$ACTIONS_DIR/config-push-fastly/scripts/config-push.sh"
  assert_fails_with "an invalid no-env value is rejected" \
    "input 'no-env' must be" \
    env PATH="$WORK_DIR/config-push/bin:$PATH" EDGEZERO__APP__CLI__BIN=fake-cli FASTLY_API_TOKEN=tok \
    GITHUB_WORKSPACE="$WORK_DIR/config-push" EDGEZERO__PROJECT__WORKING_DIRECTORY=app \
    EDGEZERO__CONFIG_PUSH__NO_ENV=yes \
    "$ACTIONS_DIR/config-push-fastly/scripts/config-push.sh"

  # The inline temp file must NOT survive the step. new_private_log installs its
  # own EXIT trap AFTER the inline-file trap, so the cleanup must be re-installed
  # — verify nothing is left in RUNNER_TEMP on the success path AND when the CLI
  # fails and the script exits via fail_with.
  local rt="$WORK_DIR/config-push/runner-temp" scenario cli
  cli="$WORK_DIR/config-push/bin/fake-cli"
  for scenario in success failure; do
    rm -rf "$rt"
    mkdir -p "$rt"
    if [[ "$scenario" == failure ]]; then
      printf '#!/usr/bin/env bash\nexit 7\n' >"$cli"
      chmod +x "$cli"
    fi
    env PATH="$WORK_DIR/config-push/bin:$PATH" FAKE_ARGV_OUT="$rt/argv.txt" \
      EDGEZERO__APP__CLI__BIN=fake-cli FASTLY_API_TOKEN=tok \
      GITHUB_WORKSPACE="$WORK_DIR/config-push" EDGEZERO__PROJECT__WORKING_DIRECTORY=app \
      RUNNER_TEMP="$rt" EDGEZERO__CONFIG_PUSH__APP_CONFIG_INLINE='a = 1' \
      "$ACTIONS_DIR/config-push-fastly/scripts/config-push.sh" >/dev/null 2>&1 || true
    assert_succeeds "the inline config temp file is removed ($scenario path)" \
      bash -c "! ls '$rt'/edgezero-inline-config.* >/dev/null 2>&1"
  done

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
      sub(/^exec[[:space:]]+/, "", line)   # a run body may exec the script
      gsub(/"/, "", line)                  # strip quoting around the path
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
      # Exception: a script may DELEGATE the run to the shared run-app-cli.sh
      # launcher (which emits `mutation-attempted` itself, right before it invokes
      # the CLI); if the step's script calls it, that counts as emitting.
      emitted=$(grep -oE "append_output ${out_name}( |\$)" "$script" || true)
      if [[ -z "$emitted" ]] && grep -q 'run-app-cli\.sh' "$script"; then
        emitted=$(grep -oE "append_output ${out_name}( |\$)" "$CORE_SCRIPTS/run-app-cli.sh" || true)
      fi
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
# capture-previous.sh — the production rollback-target capture. A first deploy
# (no active version) must yield an EMPTY previous-version and still succeed; an
# active version threads out; and an operational failure must fail CLOSED.
# ---------------------------------------------------------------------------
test_capture_previous() {
  section "capture-previous (rollback target)"
  local dir="$WORK_DIR/capture"
  rm -rf "$dir"
  mkdir -p "$dir/bin"
  cat >"$dir/bin/fake-cli" <<'CLI'
#!/usr/bin/env bash
# `active-version --help` is the credential-free preflight: it must run WITHOUT
# the token. Assert that so removing the token scrub would fail the tests.
if [ "$1" = active-version ] && [ "$2" = --help ]; then
  [ -z "${FASTLY_API_TOKEN:-}" ] || { echo "preflight must run without FASTLY_API_TOKEN" >&2; exit 91; }
  exit 0
fi
if [ "$1" = active-version ]; then
  # FAKE_SILENT models a broken CLI that exits 0 but prints no contract line.
  [ -n "${FAKE_SILENT:-}" ] || printf '%s\n' "${FAKE_VERSION_LINE-version=}"
  # FAKE_EXTRA_LINE models a CLI emitting a SECOND contract line.
  [ -z "${FAKE_EXTRA_LINE:-}" ] || printf '%s\n' "$FAKE_EXTRA_LINE"
  exit "${FAKE_EXIT:-0}"
fi
exit 3
CLI
  chmod +x "$dir/bin/fake-cli"

  run_capture() {
    : >"$dir/out.txt"
    PATH="$dir/bin:$PATH" \
      EDGEZERO__APP__CLI__BIN=fake-cli \
      EDGEZERO__FASTLY__SERVICE_ID=svc \
      FASTLY_API_TOKEN=tok \
      GITHUB_OUTPUT="$dir/out.txt" \
      FAKE_VERSION_LINE="${FVL-version=}" FAKE_EXIT="${FE:-0}" FAKE_SILENT="${FS:-}" \
      FAKE_EXTRA_LINE="${FXL:-}" \
      "$ACTIONS_DIR/deploy-fastly/scripts/capture-previous.sh"
  }
  capture_prev() { sed -nE 's/^previous-version=(.*)$/\1/p' "$dir/out.txt"; }

  # First deploy: no active version (empty `version=`) → empty target, success.
  FVL='version=' FE=0 assert_succeeds "first deploy: capture succeeds with no active version" run_capture
  assert_equals "first deploy: previous-version is empty" "" "$(capture_prev)"

  # Normal deploy: an active version threads out as previous-version.
  FVL='version=40' FE=0 assert_succeeds "capture succeeds with an active version" run_capture
  assert_equals "previous-version threads the active version" "40" "$(capture_prev)"

  # Operational failure: a non-zero active-version exit must fail CLOSED, so a
  # production deploy never proceeds with no captured rollback target.
  FVL='' FE=2 assert_fails "capture fails closed when active-version errors" run_capture

  # A silent exit-ZERO CLI (no `version=` line at all) must ALSO fail closed —
  # not be mistaken for a first deploy.
  FS=1 FE=0 assert_fails "capture fails closed on a silent exit-zero CLI" run_capture

  # A MALFORMED value (a `version=` line that isn't empty and isn't all digits)
  # must fail closed, not be silently dropped to an empty first-deploy target.
  FVL='version=12abc' FE=0 assert_fails "capture fails closed on a malformed version value" run_capture

  # ORDER must not matter: a malformed line followed by a well-formed one must
  # STILL fail closed (taking only the last line would read this as a first
  # deploy). Exactly one contract line is required.
  FVL='version=12abc' FXL='version=' FE=0 \
    assert_fails "capture fails closed when a malformed line precedes a valid one" run_capture
  FVL='version=40' FXL='version=41' FE=0 \
    assert_fails "capture fails closed on conflicting version lines" run_capture

  # A CLI without `active-version` fails the credential-free preflight early.
  printf '#!/usr/bin/env bash\nexit 2\n' >"$dir/bin/fake-cli"
  chmod +x "$dir/bin/fake-cli"
  assert_fails "capture fails when the CLI lacks active-version" run_capture
}

# ---------------------------------------------------------------------------
# healthcheck.sh — the probe path is threaded to the CLI and validated
# ---------------------------------------------------------------------------
# Runs healthcheck.sh against a fake app CLI that records its argv and emits the
# healthy verdict. Returns the recorded argv (one arg per line).
run_healthcheck_argv() {
  local dir="$WORK_DIR/healthcheck-argv"
  rm -rf "$dir"
  mkdir -p "$dir/bin"
  cat >"$dir/bin/fake-cli" <<'CLI'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$FAKE_ARGV_OUT"
echo "status-code=200"
echo "healthy=true"
CLI
  chmod +x "$dir/bin/fake-cli"
  PATH="$dir/bin:$PATH" FAKE_ARGV_OUT="$dir/argv.txt" \
    EDGEZERO__APP__CLI__BIN=fake-cli \
    EDGEZERO__LIFECYCLE__SERVICE_ID=svc123 \
    EDGEZERO__LIFECYCLE__VERSION=7 \
    EDGEZERO__LIFECYCLE__DOMAIN=www.example.com \
    EDGEZERO__LIFECYCLE__PATH="${HC_PATH:-/}" \
    EDGEZERO__DEPLOY__TO=production \
    EDGEZERO__LIFECYCLE__RETRY=1 \
    EDGEZERO__LIFECYCLE__RETRY_DELAY=0 \
    EDGEZERO__LIFECYCLE__TIMEOUT=1 \
    "$ACTIONS_DIR/healthcheck-fastly/scripts/healthcheck.sh" >/dev/null 2>&1
  cat "$dir/argv.txt" 2>/dev/null
}

test_healthcheck_path() {
  section "healthcheck probe path"
  # healthcheck.sh gates on a Linux x86-64 runner, so only there does main()
  # reach the CLI invocation whose argv we inspect. Skip elsewhere (local macOS);
  # CI's static-checks job runs on Linux and exercises it for real.
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) ;;
    *)
      pass "healthcheck path threading (skipped: non-Linux runner)"
      return
      ;;
  esac

  # Default '/' is threaded as --path.
  local default_argv after
  default_argv=$(run_healthcheck_argv)
  assert_succeeds "default path is threaded as --path" grep -qx -- '--path' <<<"$default_argv"
  after=$(grep -A1 -x -- '--path' <<<"$default_argv" | tail -n1)
  assert_equals "default --path value is '/'" "/" "$after"

  # A custom path is threaded verbatim.
  local custom_argv
  custom_argv=$(HC_PATH=/health run_healthcheck_argv)
  after=$(grep -A1 -x -- '--path' <<<"$custom_argv" | tail -n1)
  assert_equals "a custom --path value is threaded to the CLI" "/health" "$after"

  # A path without a leading slash fails closed, before the CLI is invoked.
  assert_fails "a path without a leading slash is rejected" \
    env PATH="$WORK_DIR/healthcheck-argv/bin:$PATH" EDGEZERO__APP__CLI__BIN=fake-cli \
    EDGEZERO__LIFECYCLE__SERVICE_ID=svc123 EDGEZERO__LIFECYCLE__VERSION=7 \
    EDGEZERO__LIFECYCLE__DOMAIN=www.example.com EDGEZERO__LIFECYCLE__PATH=health \
    EDGEZERO__DEPLOY__TO=production EDGEZERO__LIFECYCLE__RETRY=1 \
    EDGEZERO__LIFECYCLE__RETRY_DELAY=0 EDGEZERO__LIFECYCLE__TIMEOUT=1 \
    "$ACTIONS_DIR/healthcheck-fastly/scripts/healthcheck.sh"

  # timeout=0 becomes curl --max-time 0 (no limit): must be rejected. retry-delay=0
  # is legitimate (no wait), so only timeout is constrained to a positive integer.
  assert_fails "a zero timeout is rejected (would disable curl's timeout)" \
    env PATH="$WORK_DIR/healthcheck-argv/bin:$PATH" EDGEZERO__APP__CLI__BIN=fake-cli \
    EDGEZERO__LIFECYCLE__SERVICE_ID=svc123 EDGEZERO__LIFECYCLE__VERSION=7 \
    EDGEZERO__LIFECYCLE__DOMAIN=www.example.com EDGEZERO__LIFECYCLE__PATH=/ \
    EDGEZERO__DEPLOY__TO=production EDGEZERO__LIFECYCLE__RETRY=1 \
    EDGEZERO__LIFECYCLE__RETRY_DELAY=0 EDGEZERO__LIFECYCLE__TIMEOUT=0 \
    "$ACTIONS_DIR/healthcheck-fastly/scripts/healthcheck.sh"
}

# ---------------------------------------------------------------------------
# The mutation-attempted reconcile signal: a provider mutation whose canonical
# output line is missing STILL fails the step (loud), but the signal is already
# published (best-effort) so an `if: always()` caller can reconcile provider state.
# ---------------------------------------------------------------------------
test_mutation_attempted_signal() {
  section "mutation-attempted reconcile signal"
  local dir="$WORK_DIR/mutation-signal"
  rm -rf "$dir"
  mkdir -p "$dir/bin" "$dir/app"
  # A CLI that SUCCEEDS (exit 0) but emits no canonical line.
  printf '#!/usr/bin/env bash\nexit 0\n' >"$dir/bin/fake-cli"
  chmod +x "$dir/bin/fake-cli"

  # config-push (cross-platform): missing pushed-key fails the step, yet
  # mutation-attempted=true is already written to GITHUB_OUTPUT.
  local out="$dir/cp-out.txt" rc=0
  : >"$out"
  env PATH="$dir/bin:$PATH" EDGEZERO__APP__CLI__BIN=fake-cli FASTLY_API_TOKEN=tok \
    GITHUB_WORKSPACE="$dir" EDGEZERO__PROJECT__WORKING_DIRECTORY=app GITHUB_OUTPUT="$out" \
    "$ACTIONS_DIR/config-push-fastly/scripts/config-push.sh" >/dev/null 2>&1 || rc=$?
  assert_succeeds "config-push fails on a missing canonical line" test "$rc" -ne 0
  assert_succeeds "config-push still signals mutation-attempted on that failure" \
    grep -qx 'mutation-attempted=true' "$out"

  # rollback gates on a Linux runner; only there does main() reach the CLI.
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) ;;
    *)
      pass "rollback mutation-attempted signal (skipped: non-Linux runner)"
      return
      ;;
  esac
  local rout="$dir/rb-out.txt"
  rc=0
  : >"$rout"
  env PATH="$dir/bin:$PATH" EDGEZERO__APP__CLI__BIN=fake-cli FASTLY_API_TOKEN=tok \
    EDGEZERO__LIFECYCLE__SERVICE_ID=svc123 EDGEZERO__LIFECYCLE__VERSION=9 \
    EDGEZERO__LIFECYCLE__ROLLBACK_TO=8 EDGEZERO__DEPLOY__TO=production GITHUB_OUTPUT="$rout" \
    "$ACTIONS_DIR/rollback-fastly/scripts/rollback.sh" >/dev/null 2>&1 || rc=$?
  assert_succeeds "rollback fails on a missing rolled-back-to line" test "$rc" -ne 0
  assert_succeeds "rollback still signals mutation-attempted on that failure" \
    grep -qx 'mutation-attempted=true' "$rout"

  # A missing CLI must NOT signal a mutation — require_cmd fails before the emit.
  : >"$rout"
  rc=0
  env PATH="$dir/bin:$PATH" EDGEZERO__APP__CLI__BIN=nonexistent-cli FASTLY_API_TOKEN=tok \
    EDGEZERO__LIFECYCLE__SERVICE_ID=svc123 EDGEZERO__LIFECYCLE__VERSION=9 \
    EDGEZERO__LIFECYCLE__ROLLBACK_TO=8 EDGEZERO__DEPLOY__TO=production GITHUB_OUTPUT="$rout" \
    "$ACTIONS_DIR/rollback-fastly/scripts/rollback.sh" >/dev/null 2>&1 || rc=$?
  assert_succeeds "a rollback whose CLI is missing fails" test "$rc" -ne 0
  assert_fails "a rollback whose CLI is missing does NOT signal mutation-attempted" \
    grep -qx 'mutation-attempted=true' "$rout"

  # On a CLI failure the exit status is surfaced BEFORE any output write, so
  # rolled-back-to is not emitted (an output-write failure cannot mask the status).
  printf '#!/usr/bin/env bash\necho "rolled-back-to=8"\nexit 3\n' >"$dir/bin/fake-cli"
  chmod +x "$dir/bin/fake-cli"
  : >"$rout"
  rc=0
  env PATH="$dir/bin:$PATH" EDGEZERO__APP__CLI__BIN=fake-cli FASTLY_API_TOKEN=tok \
    EDGEZERO__LIFECYCLE__SERVICE_ID=svc123 EDGEZERO__LIFECYCLE__VERSION=9 \
    EDGEZERO__LIFECYCLE__ROLLBACK_TO=8 EDGEZERO__DEPLOY__TO=production GITHUB_OUTPUT="$rout" \
    "$ACTIONS_DIR/rollback-fastly/scripts/rollback.sh" >/dev/null 2>&1 || rc=$?
  assert_succeeds "a failing rollback CLI exits non-zero" test "$rc" -ne 0
  assert_succeeds "a failing rollback still signals mutation-attempted" \
    grep -qx 'mutation-attempted=true' "$rout"
  assert_fails "a failing rollback does NOT write rolled-back-to (status not masked)" \
    grep -qx 'rolled-back-to=8' "$rout"
}

# ---------------------------------------------------------------------------
# publish-outputs.sh — the trusted output boundary of the two-step build.
# ---------------------------------------------------------------------------
test_publish_outputs() {
  section "publish-outputs (trusted output boundary)"
  local dir="$WORK_DIR/publish"
  rm -rf "$dir"
  mkdir -p "$dir/rt"
  local pub="$ACTIONS_DIR/build-app-cli/scripts/publish-outputs.sh"
  touch "$dir/rt/edgezero-cli.tar"

  # A valid handoff, with a TAMPERED trailing duplicate tarball-path: first wins.
  {
    printf 'app-cli-version=1.2.3\n'
    printf 'app-cli-package=my-cli\n'
    printf 'app-cli-bin=my-cli\n'
    printf 'app-cli-artifact=edgezero-cli\n'
    printf 'tarball-path=%s\n' "$dir/rt/edgezero-cli.tar"
    printf 'tarball-path=/evil/hijack.tar\n'
  } >"$dir/outputs.env"
  local out="$dir/gh-output"
  : >"$out"
  RUNNER_TEMP="$dir/rt" EDGEZERO__BUILD__OUTPUTS_FILE="$dir/outputs.env" GITHUB_OUTPUT="$out" \
    bash "$pub" >/dev/null 2>&1
  assert_succeeds "publishes the legit tarball-path (first occurrence)" \
    grep -qx "tarball-path=$dir/rt/edgezero-cli.tar" "$out"
  assert_fails "ignores a tampered trailing duplicate tarball-path" \
    grep -qx 'tarball-path=/evil/hijack.tar' "$out"
  assert_succeeds "publishes the version" grep -qx 'app-cli-version=1.2.3' "$out"

  # A tarball-path escaping the action-owned temp root is refused.
  printf 'app-cli-version=1\napp-cli-package=p\napp-cli-bin=b\napp-cli-artifact=a\ntarball-path=/etc/passwd\n' \
    >"$dir/escape.env"
  assert_fails_with "a tarball-path outside RUNNER_TEMP is refused" \
    "not beneath the action-owned temp root" \
    env RUNNER_TEMP="$dir/rt" EDGEZERO__BUILD__OUTPUTS_FILE="$dir/escape.env" GITHUB_OUTPUT="$dir/o2" \
    bash "$pub"

  # A missing required output fails closed.
  printf 'app-cli-version=1\n' >"$dir/partial.env"
  assert_fails "a missing required output fails closed" \
    env RUNNER_TEMP="$dir/rt" EDGEZERO__BUILD__OUTPUTS_FILE="$dir/partial.env" GITHUB_OUTPUT="$dir/o3" \
    bash "$pub"

  # The isolation of the GitHub file-command channels is proved at RUNTIME by the
  # scrubbed-ancestor test (the re-exec strips them from the process image);
  # blanking them in the step `env:` would be an ineffective no-op (the runner
  # reinjects reserved GITHUB_* values), so this action must NOT rely on that. The
  # re-exec must strip all five — guard against dropping one.
  local common="$ACTIONS_DIR/build-app-cli/scripts/common.sh" ch
  for ch in GITHUB_ENV GITHUB_OUTPUT GITHUB_PATH GITHUB_STATE GITHUB_STEP_SUMMARY; do
    assert_succeeds "the re-exec strips $ch from the process image" \
      grep -qE "for ghvar in .*\b$ch\b" "$common"
  done
}

# ---------------------------------------------------------------------------
# cleanup_sensitive_temps — a removal FAILURE must fail the action (§13), while
# a real error code is preserved.
# ---------------------------------------------------------------------------
test_cleanup_sensitive_temps() {
  section "sensitive temp cleanup surfaces failures"
  local lib="$ACTIONS_DIR/deploy-core/scripts/common.sh"
  local dir="$WORK_DIR/cleanup-sensitive"
  rm -rf "$dir"
  mkdir -p "$dir/undeletable"

  local f="$dir/f" rc=0
  : >"$f"
  bash -c "source '$lib'; trap \"cleanup_sensitive_temps '$f'\" EXIT; exit 0" || rc=$?
  assert_succeeds "a successful cleanup preserves exit 0" test "$rc" -eq 0
  assert_fails "the sensitive file is removed" test -e "$f"

  # `rm -f` cannot remove a directory, so this deterministically fails regardless
  # of uid (root included).
  rc=0
  bash -c "source '$lib'; trap \"cleanup_sensitive_temps '$dir/undeletable'\" EXIT; exit 0" >/dev/null 2>&1 || rc=$?
  assert_succeeds "a cleanup failure on a clean exit fails the action" test "$rc" -ne 0

  rc=0
  bash -c "source '$lib'; trap \"cleanup_sensitive_temps '$dir/undeletable'\" EXIT; exit 5" >/dev/null 2>&1 || rc=$?
  assert_succeeds "a cleanup failure preserves the original non-zero exit" test "$rc" -eq 5
}

# ---------------------------------------------------------------------------
# deploy.sh — mutation-attempted must reflect ACTUAL invocation, not setup.
# ---------------------------------------------------------------------------
test_deploy_signal_timing() {
  section "deploy mutation-attempted timing"
  local dir="$WORK_DIR/deploy-signal"
  rm -rf "$dir"
  mkdir -p "$dir/bin" "$dir/app" "$dir/rt"
  # The fake CLI records whether the signal was ALREADY in GITHUB_OUTPUT when it
  # ran — proving the launcher publishes it BEFORE the mutation (so a cancel
  # mid-mutation CAN preserve it; a hard runner loss can still drop it), not after
  # the CLI returns.
  cat >"$dir/bin/fakecli" <<'CLI'
#!/usr/bin/env bash
if grep -qx 'mutation-attempted=true' "${GITHUB_OUTPUT:-/dev/null}" 2>/dev/null; then
  echo "signal-before-cli=yes" >"$PROBE"
else
  echo "signal-before-cli=no" >"$PROBE"
fi
echo "version=42"
CLI
  chmod +x "$dir/bin/fakecli"
  printf 'FASTLY_API_TOKEN\0FASTLY_SERVICE_ID\0' >"$dir/clear.nul"

  run_deploy() {
    env -i PATH="$dir/bin:$PATH" RUNNER_TEMP="$dir/rt" GITHUB_OUTPUT="$dir/out" \
      PROBE="$dir/probe" \
      EDGEZERO__FASTLY__API_TOKEN=tok EDGEZERO__FASTLY__SERVICE_ID=svc123 \
      EDGEZERO__APP__CLI__BIN="$1" EDGEZERO__ADAPTER=fastly \
      EDGEZERO__PROJECT__WORKING_DIRECTORY="$dir/app" \
      EDGEZERO__PROVIDER__ENV_CLEAR_FILE="$dir/clear.nul" \
      bash "$ACTIONS_DIR/deploy-fastly/scripts/deploy.sh"
  }

  # The CLI is invoked and succeeds: both signal and version are emitted.
  : >"$dir/out"
  : >"$dir/probe"
  assert_succeeds "a successful deploy exits 0" run_deploy fakecli
  assert_succeeds "an invoked deploy signals mutation-attempted" \
    grep -qx 'mutation-attempted=true' "$dir/out"
  assert_succeeds "an invoked deploy emits fastly-version" grep -qx 'fastly-version=42' "$dir/out"
  # Durability (best-effort): the signal was present BEFORE the CLI finished, so a
  # cancel/timeout mid-mutation CAN preserve it — though a hard runner loss can
  # still drop it, so its absence is not proof of no mutation.
  assert_equals "the signal is published before the CLI runs" \
    "signal-before-cli=yes" "$(cat "$dir/probe")"

  # Setup fails BEFORE invocation (the CLI binary is missing): NO false signal.
  : >"$dir/out"
  assert_fails "a deploy that never reaches the CLI fails" run_deploy nonexistent-bin
  assert_fails "a deploy that never reaches the CLI does NOT signal mutation-attempted" \
    grep -qx 'mutation-attempted=true' "$dir/out"
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
  test_wrapper_validate
  test_resolve_app_cli
  test_fastly_versions
  test_cleanup_confinement
  test_workspace_isolation
  test_workspace_step_scrub
  test_no_inline_action_scripts
  test_action_env_scrub
  test_deploy_args_prepend
  test_provider_env_nul
  test_lifecycle_helpers
  test_capture_previous
  test_versions_json_pins_official_release
  test_clear_provider_env_aliases
  test_toolchain_boundary
  test_config_push_argv
  test_healthcheck_path
  test_mutation_attempted_signal
  test_publish_outputs
  test_cleanup_sensitive_temps
  test_deploy_signal_timing
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
