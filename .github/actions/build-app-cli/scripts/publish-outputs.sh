#!/usr/bin/env bash
set -euo pipefail

# Emit the app-CLI build outputs to the real GITHUB_OUTPUT. This is the TRUSTED
# boundary of the two-step build: the compile step runs app-controlled code with
# every GitHub file-command channel blanked and collects its outputs into an
# action-owned file; this step (which runs NO app code) reads that file and
# re-emits each known output through the validating `append_output`.
#
# Isolating the untrusted build from the runner's command storage closes the
# /proc-derivation and duplicate-append vectors, but it is not a hard boundary
# against a fully malicious build (which can enumerate the runner's command
# directory at the same uid). This step therefore also RE-VALIDATES the handoff:
# only the known outputs are emitted, first-occurrence-wins defeats a tampered
# trailing duplicate, and `tarball-path` — which drives the artifact upload — is
# confined to the action-owned temp area and required to exist. See the guide's
# security notes: a provider secret must never share a step/job with the build.
#
# Reads (env):
#   EDGEZERO__BUILD__OUTPUTS_FILE         required  file the compile step wrote (KEY=VALUE lines)
#   RUNNER_TEMP                           optional  action-owned temp root (tarball confinement)
# Writes (outputs): app-cli-version, app-cli-package, app-cli-bin, app-cli-artifact, tarball-path

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"

# First occurrence only: the compile step writes each output exactly once, so a
# tampered LATER duplicate (which would win last-write-wins in GITHUB_OUTPUT) is
# ignored here. Fails closed if the output is absent or empty.
read_build_output() {
  local key="$1" file="$2" val
  val=$(grep -m1 -E "^${key}=" "$file" | sed -E "s/^${key}=//") || true
  [[ -n "$val" ]] || fail "the compile step did not emit a non-empty '$key'"
  printf '%s' "$val"
}

main() {
  local src="${EDGEZERO__BUILD__OUTPUTS_FILE:?EDGEZERO__BUILD__OUTPUTS_FILE is required}"
  [[ -f "$src" ]] || fail "the compile step produced no outputs file at '$src'"

  local version package bin artifact tarball
  version=$(read_build_output app-cli-version "$src")
  package=$(read_build_output app-cli-package "$src")
  bin=$(read_build_output app-cli-bin "$src")
  artifact=$(read_build_output app-cli-artifact "$src")
  tarball=$(read_build_output tarball-path "$src")

  # tarball-path drives the artifact upload, so confine it to the action-owned
  # temp area and require it to exist: a tampered handoff cannot then redirect the
  # upload to an arbitrary file.
  local runner_temp runner_real tarball_real
  runner_temp="${RUNNER_TEMP:-/tmp}"
  runner_real=$(canonical_path "$runner_temp")
  tarball_real=$(canonical_path "$tarball")
  is_under "$runner_real" "$tarball_real" ||
    fail "tarball-path is not beneath the action-owned temp root; refusing to publish it"
  [[ -f "$tarball_real" ]] || fail "tarball-path does not point at an existing file"

  append_output app-cli-version "$version"
  append_output app-cli-package "$package"
  append_output app-cli-bin "$bin"
  append_output app-cli-artifact "$artifact"
  append_output tarball-path "$tarball"
}

main "$@"
