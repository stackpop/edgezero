#!/usr/bin/env bash
#
# Install a pinned actionlint release, verified against the checksums file
# published with the same release.
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

# Work in a private temp dir that is always cleaned up.
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

curl --fail --location --silent --show-error "$base/$archive" --output "$workdir/$archive"

# Verify against the checksums file published with the same release.
curl --fail --location --silent --show-error \
  "$base/actionlint_${ACTIONLINT_VERSION}_checksums.txt" --output "$workdir/checksums.txt"
(cd "$workdir" && grep -- " ${archive}\$" checksums.txt | sha256sum --check --strict -)

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
