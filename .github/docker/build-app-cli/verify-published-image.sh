#!/usr/bin/env bash
set -euo pipefail

die() {
  printf 'verify-published-image: %s\n' "$*" >&2
  exit 1
}

ref=
local_image_id=
source_sha=
protocol=
build_metadata=
while (($#)); do
  (($# >= 2)) || die "missing value for $1"
  case "$1" in
    --ref)
      [[ -z "$ref" ]] || die "duplicate --ref"
      ref=$2
      ;;
    --source-sha)
      [[ -z "$source_sha" ]] || die "duplicate --source-sha"
      source_sha=$2
      ;;
    --protocol)
      [[ -z "$protocol" ]] || die "duplicate --protocol"
      protocol=$2
      ;;
    --local-image-id)
      [[ -z "$local_image_id" ]] || die "duplicate --local-image-id"
      local_image_id=$2
      ;;
    --build-metadata)
      [[ -z "$build_metadata" ]] || die "duplicate --build-metadata"
      build_metadata=$2
      ;;
    *) die "unknown argument: $1" ;;
  esac
  shift 2
done

repository=ghcr.io/stackpop/edgezero-build-app-cli
digest_pattern='sha256:[0-9a-f]{64}'
if [[ -n "$ref" && -n "$local_image_id" ]] || [[ -z "$ref" && -z "$local_image_id" ]]; then
  die "exactly one of --ref or --local-image-id is required"
fi
if [[ -n "$ref" ]]; then
  [[ "$ref" =~ ^${repository}@${digest_pattern}$ ]] || die "ref must be the fixed repository at a digest"
  digest=${ref#*@}
  runtime_ref=$ref
else
  [[ "$local_image_id" =~ ^${digest_pattern}$ ]] || die "local image ID must be an immutable sha256 ID"
  [[ -z "$build_metadata" ]] || die "build metadata is only valid with --ref"
  runtime_ref=$local_image_id
fi
[[ "$source_sha" =~ ^[0-9a-f]{40}$ ]] || die "source SHA must be full lowercase hex"
[[ "$protocol" == 1 ]] || die "protocol must be exactly 1"
command -v docker >/dev/null 2>&1 || die "docker is required"
command -v jq >/dev/null 2>&1 || die "jq is required"
command -v timeout >/dev/null 2>&1 || die "GNU timeout is required"

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P) || die "cannot resolve verifier directory"
fixtures="$script_dir/fixtures/provenance"
[[ -d "$fixtures/valid" && ! -L "$fixtures" && ! -L "$fixtures/valid" ]] ||
  die "trusted provenance fixtures are unavailable"

work=$(mktemp -d "/tmp/edgezero-image-verify.XXXXXX")
active_container=
docker_config="$work/docker-config"
cleanup() {
  if [[ -n "$active_container" ]]; then
    DOCKER_CONFIG="$docker_config" docker rm --force "$active_container" >/dev/null 2>&1 || true
  fi
  rm -rf -- "$work"
}
trap cleanup EXIT

if [[ -n "$build_metadata" ]]; then
  [[ -f "$build_metadata" && ! -L "$build_metadata" ]] || die "build metadata is not a regular file"
  jq -e 'type == "object"' "$build_metadata" >/dev/null 2>&1 || die "build metadata is not a JSON object"
  metadata_digest_count=$(jq --stream -r '
    select(length == 2 and (.[0] | length) == 1 and .[0][0] == "containerimage.digest") | .[1]
  ' "$build_metadata" | wc -l | tr -d ' ')
  [[ "$metadata_digest_count" == 1 ]] || die "build metadata digest is missing or duplicated"
  metadata_digest=$(jq -er '.["containerimage.digest"] | select(type == "string")' "$build_metadata") ||
    die "build metadata digest is not a string"
  [[ "$metadata_digest" == "$digest" ]] || die "build metadata digest differs from the supplied ref"
fi

mkdir -- "$docker_config"
raw="$work/manifest.json"
image="$work/image.json"
if [[ -n "$ref" ]]; then
  DOCKER_CONFIG="$docker_config" docker buildx imagetools inspect "$ref" --raw >"$raw" ||
    die "anonymous manifest inspection failed"

  jq -e --arg digest_pattern "^${digest_pattern}$" '
    type == "object" and
    .schemaVersion == 2 and
    (has("manifests") | not) and
    (
      (.mediaType == "application/vnd.oci.image.manifest.v1+json" and
        .config.mediaType == "application/vnd.oci.image.config.v1+json" and
        ([.layers[].mediaType | test("^application/vnd[.]oci[.]image[.]layer[.]v1[.]tar([+]gzip|[+]zstd)?$")] | all)) or
      (.mediaType == "application/vnd.docker.distribution.manifest.v2+json" and
        .config.mediaType == "application/vnd.docker.container.image.v1+json" and
        ([.layers[].mediaType == "application/vnd.docker.image.rootfs.diff.tar.gzip"] | all))
    ) and
    (.config | type == "object") and
    (.config.digest | type == "string" and test($digest_pattern)) and
    (.config.size | type == "number" and floor == . and . > 0) and
    (.layers | type == "array" and length > 0) and
    ([.layers[] |
      type == "object" and
      (.digest | type == "string" and test($digest_pattern)) and
      (.size | type == "number" and floor == . and . > 0)
    ] | all)
  ' "$raw" >/dev/null || die "registry object is not an accepted leaf image manifest"

  DOCKER_CONFIG="$docker_config" docker buildx imagetools inspect "$ref" \
    --format '{{json .Image}}' >"$image" || die "anonymous image-config inspection failed"
  jq -e \
    --arg source_sha "$source_sha" \
    --arg protocol "$protocol" '
    type == "object" and
    .architecture == "amd64" and
    .os == "linux" and
    .config.Entrypoint == ["/usr/bin/env"] and
    .config.User == "1001:1001" and
    .config.Labels["org.opencontainers.image.source"] == "https://github.com/stackpop/edgezero" and
    .config.Labels["org.opencontainers.image.revision"] == $source_sha and
    .config.Labels["io.edgezero.provenance-protocol"] == $protocol
  ' "$image" >/dev/null || die "image platform, config, or labels differ"

  DOCKER_CONFIG="$docker_config" docker pull "$ref" >/dev/null || die "anonymous digest pull failed"
else
  DOCKER_CONFIG="$docker_config" docker image inspect "$local_image_id" >"$image" ||
    die "local image inspection failed"
  jq -e \
    --arg image_id "$local_image_id" \
    --arg source_sha "$source_sha" \
    --arg protocol "$protocol" '
    type == "array" and length == 1 and
    .[0].Id == $image_id and
    .[0].Architecture == "amd64" and
    .[0].Os == "linux" and
    .[0].Config.Entrypoint == ["/usr/bin/env"] and
    .[0].Config.User == "1001:1001" and
    .[0].Config.Labels["org.opencontainers.image.source"] == "https://github.com/stackpop/edgezero" and
    .[0].Config.Labels["org.opencontainers.image.revision"] == $source_sha and
    .[0].Config.Labels["io.edgezero.provenance-protocol"] == $protocol
  ' "$image" >/dev/null || die "local image identity, platform, config, or labels differ"
fi

base_env="$work/base.env"
cat >"$base_env" <<'EOF'
HOME=/work/home
PATH=/usr/local/bin:/usr/local/cargo/bin:/usr/bin:/bin
TMPDIR=/work/tmp
EOF
chmod 0600 "$base_env"

probe_env="$work/probe.env"
cat >"$probe_env" <<'EOF'
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
chmod 0600 "$probe_env"

sequence=0
last_output=
last_stderr=
container_mounts=()
run_container() {
  local env_file=$1 split=$2 memory=$3 pids=$4 wall=$5
  shift 5
  sequence=$((sequence + 1))
  local name="edgezero-image-verify-$$-$sequence" container status=0
  container=$name
  last_output="$work/container-$sequence.stdout"
  last_stderr="$work/container-$sequence.stderr"
  if ! DOCKER_CONFIG="$docker_config" docker create \
    --name "$name" \
    --platform linux/amd64 \
    --user 1001:1001 \
    --read-only \
    --cap-drop=ALL \
    --security-opt=no-new-privileges \
    --network=none \
    --memory "$memory" \
    --memory-swap "$memory" \
    --pids-limit "$pids" \
    --tmpfs /work/home:rw,noexec,nosuid,nodev,mode=0700,uid=1001,gid=1001 \
    --tmpfs /work/tmp:rw,noexec,nosuid,nodev,mode=0700,uid=1001,gid=1001 \
    --env-file "$env_file" \
    ${container_mounts[@]+"${container_mounts[@]}"} \
    --entrypoint /usr/bin/env \
    "$runtime_ref" \
    -S "$split" \
    "$@" >/dev/null; then
    rm -f -- "$env_file"
    return 1
  fi
  active_container=$container
  rm -f -- "$env_file"
  [[ ! -e "$env_file" && ! -L "$env_file" ]] || die "environment file survived container creation"
  timeout --signal=TERM --kill-after=10s "$wall" \
    env DOCKER_CONFIG="$docker_config" docker start --attach "$container" \
    >"$last_output" 2>"$last_stderr" || status=$?
  if ! DOCKER_CONFIG="$docker_config" docker rm --force "$container" >/dev/null; then
    return 1
  fi
  active_container=
  return "$status"
}

run_required() {
  if ! run_container "$@"; then
    [[ ! -s "$last_stderr" ]] || cat "$last_stderr" >&2
    die "container verification failed"
  fi
}

copy_base_env() {
  local destination=$1
  cp -- "$base_env" "$destination"
  chmod 0600 "$destination"
}

assert_single_output() {
  local directory=$1 basename=$2 expected_mode=$3 path mode links count
  path="$directory/$basename"
  count=$(find "$directory" -mindepth 1 -maxdepth 1 -print | wc -l | tr -d ' ')
  [[ "$count" == 1 && -f "$path" && ! -L "$path" ]] || die "output directory shape differs"
  mode=$(stat -c '%a' "$path" 2>/dev/null || stat -f '%Lp' "$path")
  links=$(stat -c '%h' "$path" 2>/dev/null || stat -f '%l' "$path")
  [[ "$mode" == "$expected_mode" && "$links" == 1 ]] || die "output file mode or link count differs"
}

probe_split='-i EDGEZERO_ENV_BACKSLASH=${EDGEZERO_ENV_BACKSLASH} EDGEZERO_ENV_DOLLAR=${EDGEZERO_ENV_DOLLAR} EDGEZERO_ENV_EMPTY=${EDGEZERO_ENV_EMPTY} EDGEZERO_ENV_EQUALS=${EDGEZERO_ENV_EQUALS} EDGEZERO_ENV_HASH=${EDGEZERO_ENV_HASH} EDGEZERO_ENV_LITERAL=${EDGEZERO_ENV_LITERAL} EDGEZERO_ENV_NONASCII=${EDGEZERO_ENV_NONASCII} EDGEZERO_ENV_QUOTE=${EDGEZERO_ENV_QUOTE} EDGEZERO_ENV_SPACE=${EDGEZERO_ENV_SPACE} HOME=${HOME} PATH=${PATH} TMPDIR=${TMPDIR}'
run_required "$probe_env" "$probe_split" 256m 32 60s /usr/bin/env
cat >"$work/probe.expected" <<'EOF'
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
cmp -s "$last_output" "$work/probe.expected" || die "post-env target bytes differ"

copy_base_env "$work/toolchain.env"
run_required "$work/toolchain.env" '-i HOME=${HOME} PATH=${PATH} TMPDIR=${TMPDIR}' \
  2g 64 600s /usr/local/bin/verify-toolchain --root /
[[ ! -s "$last_output" ]] || die "toolchain verification produced unexpected stdout"

copy_base_env "$work/self-test.env"
run_required "$work/self-test.env" '-i HOME=${HOME} PATH=${PATH} TMPDIR=${TMPDIR}' \
  2g 64 600s /usr/local/bin/edgezero-provenance-validator self-test \
  --fixtures /usr/local/share/edgezero/provenance-fixtures
[[ ! -s "$last_output" ]] || die "validator self-test produced unexpected stdout"

fixture_expected="$fixtures/valid/expected.json"
fixture_binary="$fixtures/valid/elf-static/app-cli"
fixture_archive="$fixtures/valid/archive.tar"
for fixture in "$fixture_expected" "$fixture_binary" "$fixture_archive"; do
  [[ -f "$fixture" && ! -L "$fixture" ]] || die "required host fixture is missing or linked"
done
[[ "$work" != *,* && "$fixtures" != *,* ]] || die "fixture or temporary path is not mount-safe"

expected_dir="$work/expected"
mkdir -- "$expected_dir"
copy_base_env "$work/write-expected.env"
container_mounts=(--mount "type=bind,src=$expected_dir,dst=/work/expected")
run_required "$work/write-expected.env" '-i HOME=${HOME} PATH=${PATH} TMPDIR=${TMPDIR}' \
  256m 32 60s /usr/local/bin/edgezero-provenance-validator write-expected \
  --work-root /work \
  --app-repo-id 123456 \
  --source-revision 1111111111111111111111111111111111111111 \
  --app-cli-package edgezero-cli \
  --app-cli-bin edgezero \
  --workspace-id sha256:2222222222222222222222222222222222222222222222222222222222222222 \
  --platform-id sha256:3333333333333333333333333333333333333333333333333333333333333333 \
  --provenance-protocol 1 \
  --output /work/expected/expected.json
[[ ! -s "$last_output" ]] || die "expected writer produced unexpected stdout"
assert_single_output "$expected_dir" expected.json 644
cmp -s "$expected_dir/expected.json" "$fixture_expected" || die "expected writer bytes differ from golden fixture"

package_once() {
  local output_dir=$1 env_file=$2
  mkdir -- "$output_dir"
  copy_base_env "$env_file"
  container_mounts=(
    --mount "type=bind,src=$fixture_binary,dst=/work/input/app-cli,readonly"
    --mount "type=bind,src=$expected_dir/expected.json,dst=/work/input/expected.json,readonly"
    --mount "type=bind,src=$output_dir,dst=/work/packaged"
  )
  run_required "$env_file" '-i HOME=${HOME} PATH=${PATH} TMPDIR=${TMPDIR}' \
    2g 64 600s /usr/local/bin/edgezero-provenance-validator package \
    --work-root /work \
    --binary /work/input/app-cli \
    --schema /usr/local/share/edgezero/provenance.schema.json \
    --expected /work/input/expected.json \
    --app-cli-version 0.1.0 \
    --archive /work/packaged/artifact.tar
  [[ ! -s "$last_output" ]] || die "packager produced unexpected stdout"
  assert_single_output "$output_dir" artifact.tar 644
}

package_one="$work/package-one"
package_two="$work/package-two"
package_once "$package_one" "$work/package-one.env"
package_once "$package_two" "$work/package-two.env"
cmp -s "$package_one/artifact.tar" "$package_two/artifact.tar" || die "package output is not deterministic"
cmp -s "$package_one/artifact.tar" "$fixture_archive" || die "package output differs from golden archive"

validate_archive() {
  local archive=$1 output_dir=$2 expectation=$3 env_file="$work/validate-$((sequence + 1)).env"
  [[ -f "$archive" && ! -L "$archive" ]] || die "archive fixture is missing or linked"
  mkdir -- "$output_dir"
  copy_base_env "$env_file"
  container_mounts=(
    --mount "type=bind,src=$archive,dst=/work/input/artifact.tar,readonly"
    --mount "type=bind,src=$expected_dir/expected.json,dst=/work/input/expected.json,readonly"
    --mount "type=bind,src=$output_dir,dst=/work/validated"
  )
  if [[ "$expectation" == success ]]; then
    run_required "$env_file" '-i HOME=${HOME} PATH=${PATH} TMPDIR=${TMPDIR}' \
      2g 64 600s /usr/local/bin/edgezero-provenance-validator validate \
      --work-root /work \
      --archive /work/input/artifact.tar \
      --schema /usr/local/share/edgezero/provenance.schema.json \
      --expected /work/input/expected.json \
      --output /work/validated/app-cli
    [[ ! -s "$last_output" ]] || die "validator produced unexpected stdout"
    assert_single_output "$output_dir" app-cli 755
    cmp -s "$output_dir/app-cli" "$fixture_binary" || die "validated binary differs from packaged input"
  else
    if run_container "$env_file" '-i HOME=${HOME} PATH=${PATH} TMPDIR=${TMPDIR}' \
      2g 64 600s /usr/local/bin/edgezero-provenance-validator validate \
      --work-root /work \
      --archive /work/input/artifact.tar \
      --schema /usr/local/share/edgezero/provenance.schema.json \
      --expected /work/input/expected.json \
      --output /work/validated/app-cli; then
      die "malformed archive fixture was accepted: $archive"
    fi
    [[ -z "$(find "$output_dir" -mindepth 1 -maxdepth 1 -print -quit)" ]] ||
      die "failed validation left an output file"
  fi
}

validate_archive "$package_one/artifact.tar" "$work/validated-generated" success
validate_archive "$fixture_archive" "$work/validated-golden" success

invalid_count=0
while IFS= read -r invalid_archive; do
  invalid_count=$((invalid_count + 1))
  validate_archive "$invalid_archive" "$work/invalid-$invalid_count" failure
done < <(find "$fixtures/invalid" -mindepth 1 -maxdepth 1 -type f -name '*.tar' -print | LC_ALL=C sort)
[[ "$invalid_count" -gt 0 ]] || die "no malformed archive fixtures were exercised"
