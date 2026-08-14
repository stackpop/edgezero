#!/usr/bin/env bash
#
# Install a pinned mikefarah yq release, verified against a SHA-256 PINNED IN THIS
# REPO (not the release's own `checksums` file). yq is the pin gate's YAML parser,
# so a compromised release that served a bad binary AND a matching origin checksum
# would otherwise verify cleanly while the gate silently parses nothing. Anchoring
# the digest in committed source (the versions.json pattern) closes that. Mirrors
# scripts/install-actionlint.sh.
#
# Usage:
#   scripts/install-yq.sh <version>
#   YQ_VERSION=4.53.3 scripts/install-yq.sh
#
# Env overrides:
#   YQ_VERSION    release version, e.g. 4.53.3 (no leading "v")
#   INSTALL_DIR   install target (default: /usr/local/bin)
#   OS / ARCH     override auto-detection (e.g. linux / amd64)
set -euo pipefail

YQ_VERSION="${1:-${YQ_VERSION:-}}"
if [ -z "$YQ_VERSION" ]; then
  echo "error: set YQ_VERSION or pass the version as the first argument" >&2
  exit 2
fi
# Tolerate a leading "v".
YQ_VERSION="${YQ_VERSION#v}"

INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

# yq release assets are named `yq_<os>_<arch>` with lowercase os and amd64/arm64.
OS="${OS:-$(uname -s | tr '[:upper:]' '[:lower:]')}"
if [ -z "${ARCH:-}" ]; then
  case "$(uname -m)" in
    x86_64 | amd64) ARCH=amd64 ;;
    aarch64 | arm64) ARCH=arm64 ;;
    *)
      echo "error: unsupported architecture $(uname -m); set ARCH explicitly" >&2
      exit 1
      ;;
  esac
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{ print $1 }'
  else
    echo "error: neither sha256sum nor shasum is available" >&2
    exit 1
  fi
}

base="https://github.com/mikefarah/yq/releases/download/v${YQ_VERSION}"
asset="yq_${OS}_${ARCH}"

# Repo-pinned SHA-256 per asset. Bump these together with YQ_VERSION (copy the
# values from the release's `checksums`, verified once in a trusted context). An
# unpinned version/platform fails CLOSED rather than trusting the runtime origin.
expected=""
case "${YQ_VERSION}:${asset}" in
  4.53.3:yq_linux_amd64) expected=fa52a4e758c63d38299163fbdd1edfb4c4963247918bf9c1c5d31d84789eded4 ;;
  4.53.3:yq_linux_arm64) expected=578648e463a11c1b6db6010cbf41eafed6bee79466fcffa1bb446672cf7945ea ;;
  4.53.3:yq_darwin_amd64) expected=b4ba1ecce3c47f00803f4f964de38394326c7a32eb6540616e04fb2935a0f08d ;;
  4.53.3:yq_darwin_arm64) expected=877de31753a4dd2401aa048937aa9a7fc4d5f6ce858cf31508c5802954297213 ;;
esac
if [ -z "$expected" ]; then
  echo "error: no repo-pinned SHA-256 for $asset at yq ${YQ_VERSION}; add its digest to install-yq.sh from the release checksums rather than trusting the origin" >&2
  exit 1
fi

# Work in a private temp dir that is always cleaned up.
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

# Download to a scratch name and verify against the REPO-pinned digest BEFORE the
# binary is trusted; the origin's own `checksums` file is never consulted.
curl --fail --location --silent --show-error "$base/$asset" --output "$workdir/$asset"
actual="$(sha256_of "$workdir/$asset")"
if [ "$actual" != "$expected" ]; then
  echo "error: checksum mismatch for $asset (expected $expected, got $actual)" >&2
  exit 1
fi

# `sudo` only if we cannot write the target directly.
install_cmd=(install -m 0755 "$workdir/$asset" "$INSTALL_DIR/yq")
if [ -w "$INSTALL_DIR" ]; then
  "${install_cmd[@]}"
else
  sudo "${install_cmd[@]}"
fi

echo "yq ${YQ_VERSION} installed to ${INSTALL_DIR}/yq"
