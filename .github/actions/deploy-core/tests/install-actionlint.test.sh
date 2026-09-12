#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/install"

# Simulate release transport and extraction, recording whether unverified bytes
# reach tar. Hash results are fixtures, independent of the installer's table.
cat >"$tmp/bin/curl" <<'SH'
#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = --output ]; then shift; printf archive >"$1"; exit; fi
  shift
done
exit 1
SH
cat >"$tmp/bin/sha256sum" <<'SH'
#!/bin/sh
printf '%s  %s\n' "$TEST_DIGEST" "$1"
SH
cat >"$tmp/bin/tar" <<'SH'
#!/bin/sh
printf extracted >>"$TEST_EXTRACT_LOG"
while [ "$#" -gt 0 ]; do
  if [ "$1" = -C ]; then
    shift
    printf '#!/bin/sh\nprintf "1.7.12\\n"\n' >"$1/actionlint"
    exit
  fi
  shift
done
exit 1
SH
chmod +x "$tmp/bin/"*
export PATH="$tmp/bin:$PATH" TEST_EXTRACT_LOG="$tmp/extracted"
export INSTALL_DIR="$tmp/install"

for tuple in \
  linux:amd64:8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8 \
  linux:arm64:325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6 \
  darwin:amd64:5b44c3bc2255115c9b69e30efc0fecdf498fdb63c5d58e17084fd5f16324c644 \
  darwin:arm64:aba9ced2dee8d27fecca3dc7feb1a7f9a52caefa1eb46f3271ea66b6e0e6953f; do
  OS=${tuple%%:*}
  rest=${tuple#*:}
  ARCH=${rest%%:*}
  TEST_DIGEST=${rest#*:}
  export OS ARCH TEST_DIGEST
  bash "$root/scripts/install-actionlint.sh" 1.7.12
  [[ "$("$INSTALL_DIR/actionlint" -version)" == 1.7.12 ]]
done
before=$(wc -c <"$TEST_EXTRACT_LOG")
export TEST_DIGEST=bad
if bash "$root/scripts/install-actionlint.sh" 1.7.12 >"$tmp/result" 2>&1; then
  echo 'checksum mismatch was accepted' >&2; exit 1
fi
grep -q 'checksum mismatch' "$tmp/result"
[[ "$(wc -c <"$TEST_EXTRACT_LOG")" == "$before" ]]
for tuple in linux:riscv64 freebsd:amd64; do
  export OS=${tuple%%:*} ARCH=${tuple#*:}
  if bash "$root/scripts/install-actionlint.sh" 1.7.12 >"$tmp/result" 2>&1; then
    echo 'unknown platform was accepted' >&2; exit 1
  fi
  grep -q 'no repo-pinned SHA-256' "$tmp/result"
done
export OS=darwin ARCH=arm64
if bash "$root/scripts/install-actionlint.sh" 1.7.7 >"$tmp/result" 2>&1; then
  echo 'obsolete actionlint version was accepted' >&2; exit 1
fi
grep -q 'no repo-pinned SHA-256' "$tmp/result"
echo 'actionlint installer contract passed'
