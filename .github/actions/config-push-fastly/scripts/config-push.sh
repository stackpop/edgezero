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
# Staging: `deploy-to: staging` passes `--staging` to the CLI, which
# writes the `<key>_staging` variant in the SAME store — never the production key
# the live service reads.
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
# Writes (outputs):
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

  require_input fastly-api-token "${FASTLY_API_TOKEN:-}"
  require_cmd "$cli_bin"
  # A typo in deploy-to must never silently push to production.
  case "$deploy_to" in
    production | staging) ;;
    *) fail "input 'deploy-to' must be 'production' or 'staging' (got '$deploy_to')" ;;
  esac

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
  if [[ -n "$app_config" ]]; then app_config=$(confine_to_app "$app_config" "$app_dir" app-config); fi

  # Build the argv through a Bash array — never eval. --yes and --no-diff make the
  # push non-interactive in CI; --staging selects the `<key>_staging` variant.
  local argv=("$cli_bin" config push --adapter fastly)
  if [[ -n "$manifest" ]]; then argv+=(--manifest "$manifest"); fi
  if [[ -n "$app_config" ]]; then argv+=(--app-config "$app_config"); fi
  if [[ -n "$store" ]]; then argv+=(--store "$store"); fi
  if [[ -n "$key" ]]; then argv+=(--key "$key"); fi
  if [[ "$deploy_to" == "staging" ]]; then argv+=(--staging); fi
  argv+=(--yes --no-diff)

  new_private_log
  local rc=0
  (cd "$app_dir" && "${argv[@]}") 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    fail_with "$rc" "config push failed (CLI exit $rc)"
  fi

  # Anchored parses of the canonical lines the CLI emits. The store is whatever
  # the CLI RESOLVED from the manifest — not the optional raw input, which is
  # empty on the default path.
  local pushed resolved_store
  pushed=$(grep -oE '^pushed-key=[A-Za-z0-9._-]+$' "$LIFECYCLE_LOG" | tail -n 1 | cut -d= -f2 || true)
  resolved_store=$(grep -oE '^pushed-store=[A-Za-z0-9._-]+$' "$LIFECYCLE_LOG" | tail -n 1 | cut -d= -f2 || true)
  [[ -n "$pushed" ]] ||
    fail "config push reported success but emitted no canonical 'pushed-key=<key>' line"
  [[ -n "$resolved_store" ]] ||
    fail "config push reported success but emitted no canonical 'pushed-store=<id>' line"

  append_output pushed-key "$pushed"
  append_output store "$resolved_store"
}

main "$@"
