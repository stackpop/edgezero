#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/install"

cat >"$tmp/bin/curl" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >>"$TEST_CURL_LOG"
while [ "$#" -gt 0 ]; do
  if [ "$1" = --output ]; then
    shift
    printf '#!/bin/sh\nprintf "yq fixture\\n"\n' >"$1"
    exit 0
  fi
  shift
done
exit 1
SH
cat >"$tmp/bin/sha256sum" <<'SH'
#!/bin/sh
printf '%s  %s\n' "$TEST_DIGEST" "$1"
SH
chmod +x "$tmp/bin/"*
export PATH="$tmp/bin:$PATH" TEST_CURL_LOG="$tmp/curl.log" INSTALL_DIR="$tmp/install"

for tuple in \
  linux:amd64:fa52a4e758c63d38299163fbdd1edfb4c4963247918bf9c1c5d31d84789eded4 \
  linux:arm64:578648e463a11c1b6db6010cbf41eafed6bee79466fcffa1bb446672cf7945ea \
  darwin:amd64:b4ba1ecce3c47f00803f4f964de38394326c7a32eb6540616e04fb2935a0f08d \
  darwin:arm64:877de31753a4dd2401aa048937aa9a7fc4d5f6ce858cf31508c5802954297213; do
  OS=${tuple%%:*}
  rest=${tuple#*:}
  ARCH=${rest%%:*}
  TEST_DIGEST=${rest#*:}
  export OS ARCH TEST_DIGEST
  bash "$root/scripts/install-yq.sh" 4.53.3 >/dev/null
  grep -Fq "https://github.com/mikefarah/yq/releases/download/v4.53.3/yq_${OS}_${ARCH}" \
    "$TEST_CURL_LOG"
  [[ "$("$INSTALL_DIR/yq")" == "yq fixture" ]]
done

export OS=linux ARCH=amd64 TEST_DIGEST=bad
if bash "$root/scripts/install-yq.sh" 4.53.3 >"$tmp/result" 2>&1; then
  echo 'checksum mismatch was accepted' >&2
  exit 1
fi
grep -Fq 'checksum mismatch' "$tmp/result"

for tuple in linux:riscv64 freebsd:amd64; do
  export OS=${tuple%%:*} ARCH=${tuple#*:}
  if bash "$root/scripts/install-yq.sh" 4.53.3 >"$tmp/result" 2>&1; then
    echo 'unknown platform was accepted' >&2
    exit 1
  fi
  grep -Fq 'no repo-pinned SHA-256' "$tmp/result"
done

export OS=darwin ARCH=arm64
if bash "$root/scripts/install-yq.sh" 4.53.2 >"$tmp/result" 2>&1; then
  echo 'obsolete yq version was accepted' >&2
  exit 1
fi
grep -Fq 'no repo-pinned SHA-256' "$tmp/result"

echo 'yq installer contract passed'
