#!/usr/bin/env bash
#
# Install a pinned mikefarah yq release, verified against the SHA-256 published in
# the release's `checksums` file. Mirrors scripts/install-actionlint.sh so the pin
# gate's YAML parser is a pinned, checksum-verified validation binary — not whatever
# the runner happens to ship.
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

# Work in a private temp dir that is always cleaned up.
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

curl --fail --location --silent --show-error "$base/$asset" --output "$workdir/$asset"
curl --fail --location --silent --show-error "$base/checksums" --output "$workdir/checksums"
curl --fail --location --silent --show-error \
  "$base/checksums_hashes_order" --output "$workdir/checksums_hashes_order"

# yq's `checksums` has one column per hash algorithm; `checksums_hashes_order` gives
# the column order. Column 1 is the filename, so the SHA-256 hash sits at
# (order-index-of "SHA-256" + 1).
sha_index="$(grep -n '^SHA-256$' "$workdir/checksums_hashes_order" | head -1 | cut -d: -f1)"
if [ -z "$sha_index" ]; then
  echo "error: SHA-256 is not listed in yq's checksums_hashes_order" >&2
  exit 1
fi
expected="$(awk -v col="$((sha_index + 1))" -v name="$asset" \
  '$1 == name { print $col }' "$workdir/checksums")"
if ! printf '%s' "$expected" | grep -qE '^[0-9a-fA-F]{64}$'; then
  echo "error: could not extract a SHA-256 for $asset from yq's checksums" >&2
  exit 1
fi

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
