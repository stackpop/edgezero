#!/usr/bin/env bash
set -euo pipefail

# Pushes the application's typed config to a Fastly config store, and emits the
# key that was written.
#
# Like healthcheck.sh and rollback.sh (its sibling lifecycle actions), this calls
# the app CLI directly with FASTLY_API_TOKEN in the step env — the adapter's own
# convention, which `fastly config-store-entry update` reads to authenticate. The
# wrapper blanks every other FASTLY_* alias, so an inherited FASTLY_ENDPOINT or
# FASTLY_TOKEN can never redirect or re-auth the push.
#
# Staging: `deploy-to: staging` passes `--staging` to the CLI, which writes the
# `<logical-store-id>_staging` variant in the SAME store — the key the staging
# selector points a staged version at, never the production key the live service
# reads. `key` is production-only (the wrapper rejects key + staging up front).
#
# Path confinement: working-directory, manifest, and app-config are
# caller strings handed to a credential-bearing CLI, so each is canonicalized
# (resolving symlinks) and required to stay inside the application directory
# beneath github.workspace. Absolute paths, `..` traversal, and symlink escapes
# are rejected rather than read.
#
# Reads (env):
#   EDGEZERO__APP__CLI__PATH              optional  absolute path to the app CLI (preferred; avoids PATH shadowing)
#   EDGEZERO__APP__CLI__BIN               optional  app CLI name, used when __PATH is unset
#   FASTLY_API_TOKEN                      required  provider token (Fastly's own convention)
#   EDGEZERO__PROJECT__WORKING_DIRECTORY  required  app dir, relative to github.workspace
#   GITHUB_WORKSPACE                      required  confinement root
#   EDGEZERO__DEPLOY__TO                  optional  production | staging (default: production)
#   EDGEZERO__CONFIG_PUSH__STORE          optional  logical config-store id
#   EDGEZERO__CONFIG_PUSH__KEY            optional  explicit base key
#   EDGEZERO__CONFIG_PUSH__MANIFEST       optional  edgezero.toml path (relative to the app dir)
#   EDGEZERO__CONFIG_PUSH__APP_CONFIG     optional  typed config file path (relative to the app dir)
#   EDGEZERO__CONFIG_PUSH__APP_CONFIG_INLINE optional  raw inline typed-config content (exclusive with APP_CONFIG)
#   EDGEZERO__CONFIG_PUSH__NO_ENV         optional  'true' to pass --no-env (skip the env overlay); default false
#   RUNNER_TEMP                           optional  scratch root for the inline-config temp file (default: /tmp)
# Writes (outputs):
#   mutation-attempted                    true, emitted before the CLI runs (reconcile signal)
#   pushed-key                            the key written (base, or its _staging variant)
#   store                                 the logical store id the CLI resolved

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

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
  local key="${EDGEZERO__CONFIG_PUSH__KEY:-}"
  local manifest="${EDGEZERO__CONFIG_PUSH__MANIFEST:-}"
  local app_config="${EDGEZERO__CONFIG_PUSH__APP_CONFIG:-}"
  local app_config_inline="${EDGEZERO__CONFIG_PUSH__APP_CONFIG_INLINE:-}"
  local no_env="${EDGEZERO__CONFIG_PUSH__NO_ENV:-false}"
  local inline_file=""

  require_input fastly-api-token "${FASTLY_API_TOKEN:-}"
  require_cmd "$cli_bin"
  require_cmd git
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
  if [[ -n "$app_config" && -n "$app_config_inline" ]]; then
    fail "inputs 'app-config' and 'app-config-inline' are mutually exclusive"
  fi

  # Confine the app directory to github.workspace, then every path to the app.
  local workspace_real app_dir
  workspace_real=$(canonical_path "$workspace")
  [[ -d "$workspace/$working_directory" ]] ||
    fail "working-directory '$working_directory' does not exist or is not a directory"
  app_dir=$(canonical_path "$workspace/$working_directory")
  is_under "$workspace_real" "$app_dir" ||
    fail "input 'working-directory' must resolve inside github.workspace"
  if [[ -n "$manifest" ]]; then
    manifest=$(confine_to_app "$manifest" "$app_dir" manifest)
  elif [[ -e "$app_dir/edgezero.toml" ]]; then
    # Default discovery is confined too: the CLI reads `edgezero.toml` from the
    # app dir, and a committed symlink there could point its deploy/store config
    # outside the app while this step holds provider credentials.
    local default_manifest
    default_manifest=$(canonical_path "$app_dir/edgezero.toml")
    is_under "$app_dir" "$default_manifest" ||
      fail "the default 'edgezero.toml' resolves outside the application directory — refusing to read a manifest that escapes it"
  fi
  # Committed-source guard: config pushed from the CHECKED-OUT tree (a manifest or an
  # app-config FILE) must come from committed source, so the store the live service
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
  # push non-interactive in CI; --staging selects the `<logical>_staging` variant.
  local argv=("$cli_bin" config push --adapter fastly)
  if [[ -n "$manifest" ]]; then argv+=(--manifest "$manifest"); fi
  if [[ -n "$app_config" ]]; then argv+=(--app-config "$app_config"); fi
  if [[ -n "$store" ]]; then argv+=(--store "$store"); fi
  if [[ -n "$key" ]]; then argv+=(--key "$key"); fi
  if [[ "$deploy_to" == "staging" ]]; then argv+=(--staging); fi
  if [[ "$no_env" == "true" ]]; then argv+=(--no-env); fi
  argv+=(--yes --no-diff)

  # Enter the app dir BEFORE signalling: a directory-entry failure here means the
  # CLI was never invoked, so it must NOT falsely claim a mutation was attempted.
  cd "$app_dir" || fail "could not enter working-directory '$app_dir'"
  # Record that a provider mutation is being ATTEMPTED before the CLI runs, so the
  # signal survives a failed step (readable via `if: always()`). If the push
  # succeeds but its canonical `pushed-key=`/`pushed-store=` lines are missing
  # below, the caller can still reconcile the config store rather than assume the
  # store is unchanged.
  append_output mutation-attempted true
  local rc=0
  "${argv[@]}" 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?
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
