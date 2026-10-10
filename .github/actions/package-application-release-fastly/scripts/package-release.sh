#!/usr/bin/env bash
set -euo pipefail

# Supplies Fastly policy to the provider-neutral immutable release packager.
# The composite action owns all user-facing input mapping; this wrapper fixes the
# adapter identity, capability declaration, and Fastly package variable.
#
# Reads (env):
#   EDGEZERO__RELEASE__FASTLY_PACKAGE         required  prebuilt Fastly package
#   EDGEZERO__RELEASE__APP_CLI_ARCHIVE        forwarded to release-core
#   EDGEZERO__RELEASE__APPLICATION_MANIFEST   forwarded to release-core
#   EDGEZERO__RELEASE__ADAPTER_MANIFEST       forwarded to release-core
#   EDGEZERO__RELEASE__SOURCE_REVISION        forwarded to release-core
#   EDGEZERO__RELEASE__ARTIFACT_NAME          forwarded to release-core
# Writes:
#   the release-core package outputs unchanged

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ACTION_DIR=$(cd -- "$SCRIPT_DIR/.." && pwd)

export EDGEZERO__RELEASE__ADAPTER=fastly
export EDGEZERO__RELEASE__LIFECYCLE_CAPABILITIES="$ACTION_DIR/lifecycle-protocol.json"
export EDGEZERO__RELEASE__POLICY_ROOT="$ACTION_DIR"
export EDGEZERO__RELEASE__PACKAGE="${EDGEZERO__RELEASE__FASTLY_PACKAGE:-}"

exec "$ACTION_DIR/../release-core/scripts/package-release.sh"
