#!/usr/bin/env bash
set -euo pipefail

# Pushes the application's typed config to a Fastly config store, and emits the
# key that was written.
#
# Like its sibling lifecycle actions, this passes the token to the shared runner
# as typed data. The runner clears every Fastly alias before importing only the
# validated token, so inherited endpoint or token aliases cannot redirect or
# re-authenticate the push.
#
# Every target uses `<logical-store-id>` as the config entry key. The selected
# environment chooses the physical store through `__NAME`; using the same name
# shares config, while different names isolate it.
#
# The manifest is an absolute verified member of the immutable application
# release. Publisher-owned app-config files remain confined beneath the selected
# working directory; inline config is written to an action-owned temporary file.
#
# Reads (env):
#   EDGEZERO__APP__CLI__PATH              optional  absolute path to the app CLI (preferred; avoids PATH shadowing)
#   EDGEZERO__APP__CLI__BIN               optional  app CLI name, used when __PATH is unset
#   EDGEZERO__FASTLY__API_TOKEN           required  action-private Fastly API token
#   EDGEZERO__PROJECT__WORKING_DIRECTORY  required  app dir, relative to github.workspace
#   GITHUB_WORKSPACE                      required  confinement root
#   EDGEZERO__DEPLOY__TO                  optional  production | staging (default: production)
#   EDGEZERO__CONFIG_PUSH__STORE          optional  logical config-store id
#   EDGEZERO__CONFIG_PUSH__KEY            deprecated; nonempty is rejected
#   EDGEZERO__CONFIG_PUSH__MANIFEST       required  verified absolute release manifest
#   EDGEZERO__CONFIG_PUSH__APP_CONFIG     optional  typed config file path (relative to the app dir)
#   EDGEZERO__CONFIG_PUSH__APP_CONFIG_INLINE optional  raw inline typed-config content (exclusive with APP_CONFIG)
#   EDGEZERO__CONFIG_PUSH__NO_ENV         optional  'true' to pass --no-env (skip the env overlay); default false
#   RUNNER_TEMP                           optional  scratch root for the inline-config temp file (default: /tmp)
# Writes (outputs):
#   mutation-attempted                    true, emitted before the CLI runs (reconcile signal)
#   pushed-key                            canonical environment key, or the logical ID fallback
#   store                                 the logical store id the CLI resolved

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../fastly-common/scripts/common.sh
source "$SCRIPT_DIR/../../fastly-common/scripts/common.sh"

# Resolve a caller-supplied file path relative to the app dir and prove it stays
# inside it. Echoes the path relative to the app dir (what the CLI is given).
confine_to_app() {
  local input="$1" app_dir="$2" label="$3"
  case "$input" in
    /*) fail "input '$label' must be relative to working-directory, not absolute: '$input'" ;;
  esac
  [[ -f "$app_dir/$input" ]] || fail "input '$label' does not exist or is not a regular file: '$input'"
  local real
  real=$(canonical_path "$app_dir/$input")
  is_under "$app_dir" "$real" ||
    fail "input '$label' must resolve inside working-directory: '$input'"
  relative_to "$app_dir" "$real"
}

main() {
  local cli_bin
  cli_bin=$(resolve_app_cli)
  local working_directory="${EDGEZERO__PROJECT__WORKING_DIRECTORY:?EDGEZERO__PROJECT__WORKING_DIRECTORY is required}"
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local deploy_to="${EDGEZERO__DEPLOY__TO:-production}"
  local store="${EDGEZERO__CONFIG_PUSH__STORE:-}"
  local deprecated_key="${EDGEZERO__CONFIG_PUSH__KEY:-}"
  local manifest="${EDGEZERO__CONFIG_PUSH__MANIFEST:-}"
  local app_config="${EDGEZERO__CONFIG_PUSH__APP_CONFIG:-}"
  local app_config_inline="${EDGEZERO__CONFIG_PUSH__APP_CONFIG_INLINE:-}"
  local no_env="${EDGEZERO__CONFIG_PUSH__NO_ENV:-false}"
  local inline_file=""
  local token="${EDGEZERO__FASTLY__API_TOKEN:-}"

  if [[ -n "$deprecated_key" ]]; then
    fail "input 'key' is deprecated and unsupported; use EDGEZERO__STORES__CONFIG__<ID>__KEY"
  fi
  require_input fastly-api-token "$token"
  require_input application-manifest "$manifest"
  [[ "$manifest" == /* && -f "$manifest" && ! -L "$manifest" ]] ||
    fail "the bundled application manifest must be an absolute regular file"
  require_cmd "$cli_bin"
  cli_bin=$(command -v "$cli_bin") || fail "application CLI is unavailable"
  export EDGEZERO__APP__CLI__PATH="$cli_bin"
  require_cmd git
  require_cmd jq
  # A typo in deploy-to must never silently push to production.
  case "$deploy_to" in
    production | staging) ;;
    *) fail "input 'deploy-to' must be 'production' or 'staging' (got '$deploy_to')" ;;
  esac
  # A typo in no-env must never silently apply the env overlay the caller meant
  # to skip (which could push different values than intended).
  case "$no_env" in
    true | false) ;;
    *) fail "input 'no-env' must be 'true' or 'false' (got '$no_env')" ;;
  esac
  # A file path and inline content name the same thing two ways; requiring
  # exactly one avoids a silent precedence surprise.
  if [[ -z "$app_config" && -z "$app_config_inline" ]] ||
    [[ -n "$app_config" && -n "$app_config_inline" ]]; then
    fail "exactly one of 'app-config' or 'app-config-inline' is required"
  fi

  # Confine the app directory to github.workspace, then every path to the app.
  local workspace_real app_dir
  workspace_real=$(canonical_path "$workspace")
  [[ -d "$workspace/$working_directory" ]] ||
    fail "working-directory '$working_directory' does not exist or is not a directory"
  app_dir=$(canonical_path "$workspace/$working_directory")
  is_under "$workspace_real" "$app_dir" ||
    fail "input 'working-directory' must resolve inside github.workspace"
  # Committed-source guard: config pushed from a checked-out app-config FILE must
  # come from committed source, so the store the live service
  # reads always corresponds to a revision that can be reconciled later — the same
  # guarantee deploy gets from resolve-project.sh. Inline config is caller-supplied
  # CONTENT (a workflow variable), not the tree, so it is exempt.
  if [[ -z "$app_config_inline" ]]; then
    local git_root
    git_root=$(git -C "$app_dir" rev-parse --show-toplevel 2>/dev/null) ||
      fail "config push requires committed source, but working-directory '$working_directory' is not a Git checkout"
    git_root=$(canonical_path "$git_root")
    # Never climb above github.workspace when checking dirtiness.
    is_under "$workspace_real" "$git_root" || git_root="$workspace_real"
    assert_committed_source "$git_root" "$working_directory"
  fi

  # Open the private log and install the sensitive-temp cleanup FIRST, so there is
  # never a window where a temp file exists without a trap covering it (a cancel in
  # such a window would leave raw config behind).
  new_private_log

  # Inline config is caller-supplied CONTENT (from a GitHub variable), not a
  # checkout path, so it needs no path confinement — this step chooses the file.
  # It is written to an action-owned temp file (removed on exit) and passed by
  # ABSOLUTE path, which the CLI reads as-is. A checked-out path is confined to
  # the app directory as before.
  if [[ -n "$app_config_inline" ]]; then
    # `mktemp` (not a predictable `.$$` path): it creates the file EXCLUSIVELY with a
    # random name, so a symlink cannot be pre-planted at the path and the `>` write
    # cannot be redirected — and on a self-hosted runner where RUNNER_TEMP persists
    # across jobs, a stale/hijacked name can't be reused. The file already exists once
    # mktemp returns, so the trap set immediately after covers it (no create-then-trap
    # race). mktemp names never contain a quote, so expanding the paths into the trap
    # NOW (they must be — `inline_file` is a `main` local, out of scope at EXIT) is safe.
    inline_file=$(mktemp "${RUNNER_TEMP:-/tmp}/edgezero-inline-config.XXXXXX")
    # shellcheck disable=SC2064  # expand both paths now, not at trap time
    trap "cleanup_sensitive_temps '$LIFECYCLE_LOG' '$inline_file'" EXIT
    (
      umask 077
      printf '%s' "$app_config_inline" >"$inline_file"
    )
    app_config="$inline_file"
  elif [[ -n "$app_config" ]]; then
    app_config=$(confine_to_app "$app_config" "$app_dir" app-config)
  fi

  # Build the argv through a Bash array — never eval. --yes and --no-diff make the
  # push non-interactive in CI. --staging selects the Fastly lifecycle target;
  # it does not change the runtime config key.
  local argv=(config push --adapter fastly --manifest "$manifest" --app-config "$app_config")
  if [[ -n "$store" ]]; then argv+=(--store "$store"); fi
  if [[ "$deploy_to" == "staging" ]]; then argv+=(--staging); fi
  if [[ "$no_env" == "true" ]]; then argv+=(--no-env); fi
  argv+=(--yes --no-diff)

  local action_workspace="${EDGEZERO__ACTION__WORKSPACE:-$(dirname -- "$cli_bin")}"
  mkdir -p "$action_workspace"
  export EDGEZERO__ACTION__WORKSPACE="$action_workspace"
  local args_file="$action_workspace/config-push-argv.nul"
  local clear_file="$action_workspace/fastly-provider-clear.nul"
  printf '%s\0' "${argv[@]}" >"$args_file"
  write_fastly_provider_clear_file "$clear_file"
  EDGEZERO__PROVIDER__ENV=$(jq -n --arg token "$token" '{FASTLY_API_TOKEN:$token}')
  export EDGEZERO__PROVIDER__ENV
  export EDGEZERO__PROVIDER__ENV_CLEAR_FILE="$clear_file"
  export EDGEZERO__APP__CLI__ARGS_FILE="$args_file"
  export EDGEZERO__APP__CLI__MUTATES=true
  export EDGEZERO__PROJECT__WORKING_DIRECTORY="$app_dir"
  export EDGEZERO__PROJECT__MANIFEST_PATH="$manifest"
  local rc=0
  "$SCRIPT_DIR/../../deploy-core/scripts/run-app-cli.sh" 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    fail_with "$rc" "config push failed (CLI exit $rc)"
  fi

  # Anchored parses of the canonical lines the CLI emits. The store is whatever
  # the CLI RESOLVED from the manifest — not the optional raw input, which is
  # empty on the default path.
  #
  # The value is only anchored to a single line free of control characters — NOT a
  # narrow character class. Fastly (and the CLI) accept keys the wrapper does not
  # own (e.g. `release/canary`), so a stricter allowlist would reject a key AFTER
  # the CLI already wrote it. `append_output` still rejects an embedded newline.
  local pushed resolved_store
  pushed=$(grep -oE '^pushed-key=[^[:cntrl:]]+$' "$LIFECYCLE_LOG" | tail -n 1 | sed 's/^pushed-key=//' || true)
  resolved_store=$(grep -oE '^pushed-store=[^[:cntrl:]]+$' "$LIFECYCLE_LOG" | tail -n 1 | sed 's/^pushed-store=//' || true)
  [[ -n "$pushed" ]] ||
    fail "config push reported success but emitted no canonical 'pushed-key=<key>' line"
  [[ -n "$resolved_store" ]] ||
    fail "config push reported success but emitted no canonical 'pushed-store=<id>' line"

  append_output pushed-key "$pushed"
  append_output store "$resolved_store"
}

main "$@"
