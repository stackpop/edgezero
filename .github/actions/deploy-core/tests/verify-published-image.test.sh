#!/usr/bin/env bash
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
VERIFY="$DIR/../../../docker/build-app-cli/verify-published-image.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
BIN="$WORK/bin"
STATE="$WORK/state"
mkdir "$BIN" "$STATE"

pass=0
fail=0
ok() {
  printf '  \033[32mok\033[0m   %s\n' "$1"
  pass=$((pass + 1))
}
no() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1" >&2
  fail=$((fail + 1))
}
assert_pass() {
  local description=$1
  shift
  if "$@" >/dev/null 2>&1; then ok "$description"; else no "$description"; fi
}
assert_fail() {
  local description=$1
  shift
  if "$@" >/dev/null 2>&1; then no "$description"; else ok "$description"; fi
}

REF=ghcr.io/stackpop/edgezero-build-app-cli@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
LOCAL_ID=sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd
SOURCE=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

cat >"$BIN/timeout" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
while (($#)) && [[ "$1" == --* ]]; do
  shift
done
(($# >= 2)) || exit 125
shift
exec "$@"
EOF
chmod 0755 "$BIN/timeout"

cat >"$BIN/docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\0' "$@" >>"$FAKE_STATE/docker.log"
printf '\n' >>"$FAKE_STATE/docker.log"
[[ -n "${DOCKER_CONFIG:-}" && -d "$DOCKER_CONFIG" && ! -e "$DOCKER_CONFIG/config.json" ]] || exit 96

if [[ "$1 ${2:-} ${3:-}" == "buildx imagetools inspect" ]]; then
  [[ "${FAKE_INSPECT_STATUS:-0}" == 0 ]] || exit "$FAKE_INSPECT_STATUS"
  if [[ "$5" == "--raw" ]]; then
    cat "$FAKE_STATE/raw.json"
  else
    [[ "$5" == "--format" && "$6" == "{{json .Image}}" ]]
    cat "$FAKE_STATE/image.json"
  fi
  exit 0
fi

case "$1" in
  image)
    [[ "$2" == inspect && "$3" == "$FAKE_LOCAL_ID" ]]
    cat "$FAKE_STATE/local-image.json"
    ;;
  pull)
    [[ "${FAKE_PULL_STATUS:-0}" == 0 ]] || exit "$FAKE_PULL_STATUS"
    ;;
  create)
    count=$(cat "$FAKE_STATE/create-count")
    count=$((count + 1))
    printf '%s\n' "$count" >"$FAKE_STATE/create-count"
    args="$FAKE_STATE/create-$count.args"
    printf '%s\n' "${@:2}" >"$args"
    previous=
    for argument in "${@:2}"; do
      if [[ "$previous" == --env-file ]]; then
        cp "$argument" "$FAKE_STATE/create-$count.env"
        printf '%s\n' "$argument" >"$FAKE_STATE/create-$count.env-path"
      fi
      case "$argument" in
        type=bind,src=*,dst=/work/expected)
          value=${argument#type=bind,src=}
          printf '%s\n' "${value%,dst=/work/expected}" >"$FAKE_STATE/create-$count.mount-expected"
          ;;
        type=bind,src=*,dst=/work/input/app-cli,readonly)
          value=${argument#type=bind,src=}
          printf '%s\n' "${value%,dst=/work/input/app-cli,readonly}" >"$FAKE_STATE/create-$count.mount-app-cli"
          ;;
        type=bind,src=*,dst=/work/input/expected.json,readonly)
          value=${argument#type=bind,src=}
          printf '%s\n' "${value%,dst=/work/input/expected.json,readonly}" >"$FAKE_STATE/create-$count.mount-expected-json"
          ;;
        type=bind,src=*,dst=/work/packaged)
          value=${argument#type=bind,src=}
          printf '%s\n' "${value%,dst=/work/packaged}" >"$FAKE_STATE/create-$count.mount-packaged"
          ;;
        type=bind,src=*,dst=/work/input/artifact.tar,readonly)
          value=${argument#type=bind,src=}
          printf '%s\n' "${value%,dst=/work/input/artifact.tar,readonly}" >"$FAKE_STATE/create-$count.mount-artifact"
          ;;
        type=bind,src=*,dst=/work/validated)
          value=${argument#type=bind,src=}
          printf '%s\n' "${value%,dst=/work/validated}" >"$FAKE_STATE/create-$count.mount-validated"
          ;;
      esac
      previous=$argument
    done
    printf '%s\n' "${FAKE_CREATE_OUTPUT:-container-$count}"
    ;;
  start)
    id=${3##*-}
    env_path=$(cat "$FAKE_STATE/create-$id.env-path")
    [[ ! -e "$env_path" && ! -L "$env_path" ]] || exit 97
    case "$id" in
      1)
        cat "$FAKE_STATE/env-output"
        [[ "${FAKE_ENV_STATUS:-0}" == 0 ]] || exit "$FAKE_ENV_STATUS"
        ;;
      2) [[ "${FAKE_TOOLCHAIN_STATUS:-0}" == 0 ]] || exit "$FAKE_TOOLCHAIN_STATUS" ;;
      3) [[ "${FAKE_SELF_TEST_STATUS:-0}" == 0 ]] || exit "$FAKE_SELF_TEST_STATUS" ;;
      *)
        if grep -Fxq write-expected "$FAKE_STATE/create-$id.args"; then
          cp "$FAKE_FIXTURES/valid/expected.json" \
            "$(cat "$FAKE_STATE/create-$id.mount-expected")/expected.json"
        elif grep -Fxq package "$FAKE_STATE/create-$id.args"; then
          cp "$FAKE_FIXTURES/valid/archive.tar" \
            "$(cat "$FAKE_STATE/create-$id.mount-packaged")/artifact.tar"
        elif grep -Fxq validate "$FAKE_STATE/create-$id.args"; then
          archive=$(cat "$FAKE_STATE/create-$id.mount-artifact")
          [[ "$archive" != */invalid/* ]] || exit 1
          cp "$FAKE_FIXTURES/valid/elf-static/app-cli" \
            "$(cat "$FAKE_STATE/create-$id.mount-validated")/app-cli"
          chmod 0755 "$(cat "$FAKE_STATE/create-$id.mount-validated")/app-cli"
        else
          exit 98
        fi
        ;;
    esac
    ;;
  rm) ;;
  *) exit 99 ;;
esac
EOF
chmod 0755 "$BIN/docker"

write_oci_manifest() {
  cat >"$STATE/raw.json" <<'EOF'
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","size":123,"digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111"},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","size":456,"digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"}]}
EOF
}

write_docker_manifest() {
  cat >"$STATE/raw.json" <<'EOF'
{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{"mediaType":"application/vnd.docker.container.image.v1+json","size":123,"digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111"},"layers":[{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","size":456,"digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222"}]}
EOF
}

write_image() {
  cat >"$STATE/image.json" <<EOF
{"architecture":"amd64","os":"linux","config":{"Entrypoint":["/usr/bin/env"],"User":"1001:1001","Labels":{"io.edgezero.provenance-protocol":"1","org.opencontainers.image.revision":"$SOURCE","org.opencontainers.image.source":"https://github.com/stackpop/edgezero"}}}
EOF
}

write_local_image() {
  cat >"$STATE/local-image.json" <<EOF
[{"Id":"$LOCAL_ID","Architecture":"amd64","Os":"linux","Config":{"Entrypoint":["/usr/bin/env"],"User":"1001:1001","Labels":{"io.edgezero.provenance-protocol":"1","org.opencontainers.image.revision":"$SOURCE","org.opencontainers.image.source":"https://github.com/stackpop/edgezero"}}}]
EOF
}

reset_state() {
  rm -f "$STATE"/create-* "$STATE/docker.log"
  : >"$STATE/docker.log"
  mkdir -p "$STATE/hostile-docker-config"
  printf '{"auths":{"ghcr.io":{"auth":"hostile"}}}\n' >"$STATE/hostile-docker-config/config.json"
  printf '0\n' >"$STATE/create-count"
  write_oci_manifest
  write_image
  write_local_image
  cat >"$STATE/env-output" <<'EOF'
EDGEZERO_ENV_BACKSLASH=back\slash
EDGEZERO_ENV_DOLLAR=dollar$value
EDGEZERO_ENV_EMPTY=
EDGEZERO_ENV_EQUALS=left=right
EDGEZERO_ENV_HASH=hash#value
EDGEZERO_ENV_LITERAL=${EDGEZERO_ENV_DOLLAR}
EDGEZERO_ENV_NONASCII=café
EDGEZERO_ENV_QUOTE=quote"value
EDGEZERO_ENV_SPACE=two words
HOME=/work/home
PATH=/usr/local/bin:/usr/local/cargo/bin:/usr/bin:/bin
TMPDIR=/work/tmp
EOF
  printf '{"containerimage.digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}\n' \
    >"$STATE/metadata.json"
}

run_verify() {
  PATH="$BIN:$PATH" \
    FAKE_STATE="$STATE" \
    FAKE_INSPECT_STATUS="${FAKE_INSPECT_STATUS:-0}" \
    FAKE_PULL_STATUS="${FAKE_PULL_STATUS:-0}" \
    FAKE_ENV_STATUS="${FAKE_ENV_STATUS:-0}" \
    FAKE_TOOLCHAIN_STATUS="${FAKE_TOOLCHAIN_STATUS:-0}" \
    FAKE_SELF_TEST_STATUS="${FAKE_SELF_TEST_STATUS:-0}" \
    FAKE_CREATE_OUTPUT="${FAKE_CREATE_OUTPUT:-}" \
    FAKE_LOCAL_ID="$LOCAL_ID" \
    FAKE_FIXTURES="$DIR/../../../docker/build-app-cli/fixtures/provenance" \
    DOCKER_CONFIG="$STATE/hostile-docker-config" \
    bash "$VERIFY" \
    --ref "$REF" \
    --source-sha "$SOURCE" \
    --protocol 1 \
    --build-metadata "$STATE/metadata.json"
}


run_verify_local() {
  PATH="$BIN:$PATH" \
    FAKE_STATE="$STATE" \
    FAKE_ENV_STATUS="${FAKE_ENV_STATUS:-0}" \
    FAKE_TOOLCHAIN_STATUS="${FAKE_TOOLCHAIN_STATUS:-0}" \
    FAKE_SELF_TEST_STATUS="${FAKE_SELF_TEST_STATUS:-0}" \
    FAKE_CREATE_OUTPUT="${FAKE_CREATE_OUTPUT:-}" \
    FAKE_LOCAL_ID="$LOCAL_ID" \
    FAKE_FIXTURES="$DIR/../../../docker/build-app-cli/fixtures/provenance" \
    DOCKER_CONFIG="$STATE/hostile-docker-config" \
    bash "$VERIFY" \
    --local-image-id "$LOCAL_ID" \
    --source-sha "$SOURCE" \
    --protocol 1
}

echo "== published build-container verification =="

reset_state
assert_pass "an OCI leaf with exact platform, labels, and runtime passes" run_verify

reset_state
assert_pass "an immutable local BuildKit image ID passes without registry access" run_verify_local
tr '\0' '\n' <"$STATE/docker.log" >"$STATE/docker.lines"
if grep -Eq '^(buildx|pull)$' "$STATE/docker.lines"; then
  no "local image verification avoids registry inspection and pull"
else
  ok "local image verification avoids registry inspection and pull"
fi

reset_state
LOCAL_ID=edgezero-build-app-cli:bootstrap
assert_fail "a tagged local image reference is rejected" run_verify_local
LOCAL_ID=sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd

reset_state
assert_fail "local and published image identities are mutually exclusive" \
  bash "$VERIFY" --ref "$REF" --local-image-id "$LOCAL_ID" --source-sha "$SOURCE" --protocol 1

reset_state
write_docker_manifest
assert_pass "a Docker v2 leaf is accepted" run_verify

reset_state
cat >"$STATE/raw.json" <<'EOF'
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","mediaType":"application/vnd.oci.image.manifest.v1+json","size":1,"platform":{"architecture":"amd64","os":"linux"}}]}
EOF
assert_fail "a one-entry OCI index is rejected" run_verify

reset_state
cat >"$STATE/raw.json" <<'EOF'
{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.list.v2+json","manifests":[{},{}]}
EOF
assert_fail "a multi-entry Docker manifest list is rejected" run_verify

reset_state
jq 'del(.config)' "$STATE/raw.json" >"$STATE/raw.new"
mv "$STATE/raw.new" "$STATE/raw.json"
assert_fail "a leaf without config is rejected" run_verify

reset_state
jq 'del(.layers)' "$STATE/raw.json" >"$STATE/raw.new"
mv "$STATE/raw.new" "$STATE/raw.json"
assert_fail "a leaf without layers is rejected" run_verify

for mutation in \
  '.architecture = "arm64"' \
  '.os = "windows"' \
  '.config.Labels["org.opencontainers.image.source"] = "https://attacker.invalid/repo"' \
  '.config.Labels["org.opencontainers.image.revision"] = "cccccccccccccccccccccccccccccccccccccccc"' \
  '.config.Labels["io.edgezero.provenance-protocol"] = "2"' \
  '.config.Entrypoint = ["/bin/sh"]' \
  '.config.User = "0:0"'; do
  reset_state
  jq "$mutation" "$STATE/image.json" >"$STATE/image.new"
  mv "$STATE/image.new" "$STATE/image.json"
  assert_fail "image config mutation is rejected: $mutation" run_verify
done

reset_state
REF=ghcr.io/stackpop/edgezero-build-app-cli:build-container-v1
assert_fail "a mutable tag lookup is rejected" run_verify
REF=ghcr.io/stackpop/edgezero-build-app-cli@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

reset_state
FAKE_INSPECT_STATUS=1
assert_fail "an unavailable or private registry response is rejected" run_verify
unset FAKE_INSPECT_STATUS

for metadata in \
  'not-json' \
  '{}' \
  '{"containerimage.digest":1}' \
  '{"containerimage.digest":"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}' \
  '{"containerimage.digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","containerimage.digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}'; do
  reset_state
  printf '%s\n' "$metadata" >"$STATE/metadata.json"
  assert_fail "malformed or contradictory BuildKit metadata is rejected: $metadata" run_verify
done

reset_state
FAKE_PULL_STATUS=1
assert_fail "an anonymous pull failure is rejected" run_verify
unset FAKE_PULL_STATUS

reset_state
FAKE_ENV_STATUS=1
assert_fail "the real env-file capability probe must pass" run_verify
unset FAKE_ENV_STATUS

reset_state
FAKE_CREATE_OUTPUT='daemon-output-is-not-container-authority'
assert_pass "daemon create output cannot redirect start or cleanup" run_verify
unset FAKE_CREATE_OUTPUT

reset_state
printf 'HOST_POISON=must-not-survive\n' >>"$STATE/env-output"
assert_fail "an inherited image or Docker environment name is rejected" run_verify

reset_state
FAKE_TOOLCHAIN_STATUS=1
assert_fail "a toolchain verification failure propagates" run_verify
unset FAKE_TOOLCHAIN_STATUS

reset_state
FAKE_SELF_TEST_STATUS=1
assert_fail "a validator self-test failure propagates" run_verify
unset FAKE_SELF_TEST_STATUS

reset_state
run_verify >/dev/null
[[ "$(cat "$STATE/create-count")" -gt 8 ]] && ok "the verifier exercises every baked archive fixture" ||
  no "the verifier exercises every baked archive fixture"
if [[ "$(cat "$STATE/create-3.env")" == $'HOME=/work/home\nPATH=/usr/local/bin:/usr/local/cargo/bin:/usr/bin:/bin\nTMPDIR=/work/tmp' ]]; then
  ok "self-test receives exactly the sorted three-variable environment"
else
  no "self-test receives exactly the sorted three-variable environment"
fi
if grep -RFxq -- '/work/package' "$STATE"/create-*.args; then
  no "no verifier profile uses the forbidden /work/package convention"
else
  ok "no verifier profile uses the forbidden /work/package convention"
fi
awk -v ref="$REF" '
  previous == "--name" { $0 = "<name>" }
  previous == "--env-file" { $0 = "<env-file>" }
  $0 == ref { $0 = "<ref>" }
  { print; previous = $0 }
' "$STATE/create-3.args" >"$STATE/create-3.normalized"
cat >"$STATE/create-3.expected" <<'EOF'
--name
<name>
--platform
linux/amd64
--user
1001:1001
--read-only
--cap-drop=ALL
--security-opt=no-new-privileges
--network=none
--memory
2g
--memory-swap
2g
--pids-limit
64
--tmpfs
/work/home:rw,noexec,nosuid,nodev,mode=0700,uid=1001,gid=1001
--tmpfs
/work/tmp:rw,noexec,nosuid,nodev,mode=0700,uid=1001,gid=1001
--env-file
<env-file>
--entrypoint
/usr/bin/env
<ref>
-S
-i HOME=${HOME} PATH=${PATH} TMPDIR=${TMPDIR}
/usr/local/bin/edgezero-provenance-validator
self-test
--fixtures
/usr/local/share/edgezero/provenance-fixtures
EOF
if cmp -s "$STATE/create-3.normalized" "$STATE/create-3.expected"; then
  ok "self-test create argv has the exact hardened runtime contract"
else
  diff -u "$STATE/create-3.expected" "$STATE/create-3.normalized" >&2 || true
  no "self-test create argv has the exact hardened runtime contract"
fi

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
