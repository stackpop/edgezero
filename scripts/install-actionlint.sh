#!/usr/bin/env bash
#
# Install a pinned actionlint release, verified against a SHA-256 PINNED IN THIS
# REPO — not the release's own checksums file. actionlint is a validation binary the
# gate trusts, so a compromised release serving a bad archive AND a matching origin
# checksum would otherwise verify cleanly. Anchoring the digest in committed source
# (the versions.json pattern) closes that.
#
# Usage:
#   scripts/install-actionlint.sh <version>
#   ACTIONLINT_VERSION=1.7.7 scripts/install-actionlint.sh
#
# Env overrides:
#   ACTIONLINT_VERSION   release version, e.g. 1.7.7 (no leading "v")
#   INSTALL_DIR          install target (default: /usr/local/bin)
#   OS / ARCH            override auto-detection (e.g. linux / amd64)
set -euo pipefail

ACTIONLINT_VERSION="${1:-${ACTIONLINT_VERSION:-}}"
if [ -z "$ACTIONLINT_VERSION" ]; then
  echo "error: set ACTIONLINT_VERSION or pass the version as the first argument" >&2
  exit 2
fi
# Tolerate a leading "v".
ACTIONLINT_VERSION="${ACTIONLINT_VERSION#v}"

INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

# Detect platform unless overridden. actionlint uses lowercase os and
# amd64/arm64 arch names.
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

base="https://github.com/rhysd/actionlint/releases/download/v${ACTIONLINT_VERSION}"
archive="actionlint_${ACTIONLINT_VERSION}_${OS}_${ARCH}.tar.gz"

# Repo-pinned SHA-256 per archive. Bump together with ACTIONLINT_VERSION (copy from
# the release's checksums, verified once in a trusted context). An unpinned
# version/platform fails CLOSED rather than trusting the runtime origin.
expected=""
case "${ACTIONLINT_VERSION}:${archive}" in
  1.7.7:actionlint_1.7.7_linux_amd64.tar.gz) expected=023070a287cd8cccd71515fedc843f1985bf96c436b7effaecce67290e7e0757 ;;
  1.7.7:actionlint_1.7.7_linux_arm64.tar.gz) expected=401942f9c24ed71e4fe71b76c7d638f66d8633575c4016efd2977ce7c28317d0 ;;
  1.7.7:actionlint_1.7.7_darwin_amd64.tar.gz) expected=28e5de5a05fc558474f638323d736d822fff183d2d492f0aecb2b73cc44584f5 ;;
  1.7.7:actionlint_1.7.7_darwin_arm64.tar.gz) expected=2693315b9093aeacb4ebd91a993fea54fc215057bf0da2659056b4bc033873db ;;
esac
if [ -z "$expected" ]; then
  echo "error: no repo-pinned SHA-256 for $archive at actionlint ${ACTIONLINT_VERSION}; add its digest to install-actionlint.sh from the release checksums rather than trusting the origin" >&2
  exit 1
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

# Work in a private temp dir that is always cleaned up.
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

# Download, then verify against the REPO-pinned digest before the archive is trusted;
# the origin's own checksums file is never consulted.
curl --fail --location --silent --show-error "$base/$archive" --output "$workdir/$archive"
actual="$(sha256_of "$workdir/$archive")"
if [ "$actual" != "$expected" ]; then
  echo "error: checksum mismatch for $archive (expected $expected, got $actual)" >&2
  exit 1
fi

tar -xzf "$workdir/$archive" -C "$workdir" actionlint

# `sudo` only if we cannot write the target directly (e.g. running as root, or
# an INSTALL_DIR the user owns).
install_cmd=(install -m 0755 "$workdir/actionlint" "$INSTALL_DIR/actionlint")
if [ -w "$INSTALL_DIR" ]; then
  "${install_cmd[@]}"
else
  sudo "${install_cmd[@]}"
fi

echo "actionlint ${ACTIONLINT_VERSION} installed to ${INSTALL_DIR}/actionlint"
