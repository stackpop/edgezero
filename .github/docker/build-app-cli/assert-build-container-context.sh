#!/usr/bin/env bash
set -euo pipefail

die() {
  printf 'assert-build-container-context: %s\n' "$*" >&2
  exit 1
}

context=
while (($#)); do
  (($# >= 2)) || die "missing value for $1"
  case "$1" in
    --context)
      [[ -z "$context" ]] || die "duplicate --context"
      context=$2
      ;;
    *) die "unknown argument: $1" ;;
  esac
  shift 2
done

[[ -n "$context" ]] || die "--context is required"
context=$(cd -- "$context" 2>/dev/null && pwd -P) || die "context is not a directory"

manifest=.github/docker/build-app-cli/image-context-paths.txt
dockerfile=.github/docker/build-app-cli/Dockerfile
validator=.github/tools/edgezero-provenance-validator
[[ -f "$context/$manifest" && ! -L "$context/$manifest" ]] || die "context manifest is not regular"
[[ -f "$context/$dockerfile" && ! -L "$context/$dockerfile" ]] || die "Dockerfile is not regular"

valid_path() {
  local path=$1 component
  [[ -n "$path" && "$path" != /* && "$path" != */ && "$path" != *//* && "$path" != *\\* ]] ||
    return 1
  while IFS= read -r component; do
    [[ -n "$component" && "$component" != "." && "$component" != ".." ]] || return 1
  done < <(printf '%s' "$path" | tr '/' '\n')
}

previous=''
while IFS= read -r path; do
  valid_path "$path" || die "unsafe manifest path"
  [[ -z "$previous" || "$previous" < "$path" ]] || die "manifest is not sorted and unique"
  previous=$path
done <"$context/$manifest"
[[ -n "$previous" ]] || die "manifest is empty"
[[ "$(tail -c 1 "$context/$manifest" | wc -l | tr -d ' ')" == "1" ]] ||
  die "manifest lacks one final newline"

expected=$(mktemp "${TMPDIR:-/tmp}/edgezero-image-inputs.XXXXXX")
actual=$(mktemp "${TMPDIR:-/tmp}/edgezero-image-files.XXXXXX")
metadata=$(mktemp "${TMPDIR:-/tmp}/edgezero-image-metadata.XXXXXX")
trap 'rm -f "$expected" "$actual" "$metadata"' EXIT

{
  printf '%s\n' \
    .dockerignore \
    .github/actions/deploy-fastly/versions.json \
    .github/docker/build-app-cli/Dockerfile \
    .github/docker/build-app-cli/fixtures/gnu-smoke.rs \
    .github/docker/build-app-cli/fixtures/wasm-smoke.rs \
    .github/docker/build-app-cli/image-context-paths.txt \
    .github/docker/build-app-cli/provenance.schema.json \
    .github/docker/build-app-cli/verify-toolchain.sh \
    .tool-versions
  find "$context/.github/docker/build-app-cli/fixtures/provenance" \
    "$context/$validator" -type f -print | sed "s#^$context/##"
} | LC_ALL=C sort >"$expected"
cmp -s "$expected" "$context/$manifest" || die "manifest does not close over exact image inputs"

find "$context" \( -type f -o -type l \) -print | sed "s#^$context/##" | LC_ALL=C sort >"$actual"
cmp -s "$actual" "$context/$manifest" || die "context inventory differs from manifest"

link_count() {
  if stat -c '%h' -- "$1" >/dev/null 2>&1; then
    stat -c '%h' -- "$1"
  else
    stat -f '%l' -- "$1"
  fi
}

while IFS= read -r path; do
  [[ -f "$context/$path" && ! -L "$context/$path" ]] || die "$path is not a regular file"
  [[ "$(link_count "$context/$path")" == "1" ]] || die "$path has multiple links"
done <"$context/$manifest"
[[ -x "$context/.github/docker/build-app-cli/verify-toolchain.sh" ]] ||
  die "verify-toolchain.sh is not executable"

cargo metadata --locked --manifest-path "$context/$validator/Cargo.toml" --format-version 1 \
  >"$metadata"
validator_root=$(cd -- "$context/$validator" && pwd -P)
jq -e --arg root "$validator_root" '
  .workspace_root == $root and
  (.workspace_members | length >= 1) and
  ([.packages[] | select(.source == null) | .manifest_path |
    startswith($root + "/") or . == ($root + "/Cargo.toml")] | all)
' "$metadata" >/dev/null || die "validator workspace or path dependency escapes its directory"

df="$context/$dockerfile"
! grep -Eq '^#[[:space:]]*syntax=' "$df" || die "external Dockerfile frontend is forbidden"
base='FROM docker.io/library/rust@sha256:6f9e63259f12e1e599296f5ecfed2bae46de4af0ee0525dd8b89c046e236d5c5'
[[ "$(grep -Fxc "$base AS builder" "$df")" == "1" ]] || die "builder base is not exact"
[[ "$(grep -Fxc "$base AS runtime" "$df")" == "1" ]] || die "runtime base is not exact"
[[ "$(grep -Ec '^[[:space:]]*FROM[[:space:]]' "$df")" == "2" ]] || die "unexpected FROM instruction"
grep -Fqx "RUN test \"\${#IMAGE_SOURCE_REVISION}\" -eq 40 \\" "$df" ||
  die "source revision length guard is missing"
grep -Fqx "    && case \"\$IMAGE_SOURCE_REVISION\" in *[!0-9a-f]*) exit 1 ;; esac" "$df" ||
  die "source revision alphabet guard is missing"
! grep -Eq '^[[:space:]]*ADD([[:space:]]|$)' "$df" || die "ADD is forbidden"
! grep -Eq '^[[:space:]]*RUN[[:space:]]+--mount=.*type=bind' "$df" ||
  die "build-context bind mounts are forbidden"
! grep -Eq '^[[:space:]]*COPY[[:space:]]+(--[^[:space:]]+[[:space:]]+)*\.[[:space:]]' "$df" ||
  die "broad context COPY is forbidden"
! grep -Eq '^[[:space:]]*ONBUILD([[:space:]]|$)' "$df" || die "ONBUILD is forbidden"

build='RUN cargo build --locked --release --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml'
[[ "$(grep -Fxc "$build" "$df")" == "1" ]] || die "validator must have one exact build invocation"
[[ "$(grep -Foc 'cargo build' "$df")" == "1" ]] || die "unexpected cargo build invocation"

expected_copies=$(cat <<'EOF'
COPY .tool-versions /edgezero-input/.tool-versions
COPY .github/actions/deploy-fastly/versions.json /edgezero-input/fastly-versions.json
COPY .github/tools/edgezero-provenance-validator/Cargo.toml .github/tools/edgezero-provenance-validator/Cargo.lock .github/tools/edgezero-provenance-validator/
COPY .github/tools/edgezero-provenance-validator/src .github/tools/edgezero-provenance-validator/src
COPY .github/tools/edgezero-provenance-validator/tests .github/tools/edgezero-provenance-validator/tests
COPY --from=builder /usr/local/bin/fastly /usr/local/bin/fastly
COPY --from=builder /usr/local/bin/sccache /usr/local/bin/sccache
COPY --from=builder /build/.github/tools/edgezero-provenance-validator/target/release/edgezero-provenance-validator /usr/local/bin/edgezero-provenance-validator
COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/components /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/components
COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/manifest-rust-std-wasm32-wasip1 /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/manifest-rust-std-wasm32-wasip1
COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/multirust-config.toml /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/multirust-config.toml
COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/wasm32-wasip1 /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/wasm32-wasip1
COPY .tool-versions /usr/local/share/edgezero/.tool-versions
COPY .github/actions/deploy-fastly/versions.json /usr/local/share/edgezero/fastly-versions.json
COPY .github/docker/build-app-cli/provenance.schema.json /usr/local/share/edgezero/provenance.schema.json
COPY .github/docker/build-app-cli/fixtures/provenance /usr/local/share/edgezero/provenance-fixtures
COPY .github/docker/build-app-cli/fixtures/gnu-smoke.rs /usr/local/share/edgezero/gnu-smoke.rs
COPY .github/docker/build-app-cli/fixtures/wasm-smoke.rs /usr/local/share/edgezero/wasm-smoke.rs
COPY .github/docker/build-app-cli/verify-toolchain.sh /usr/local/bin/verify-toolchain
EOF
)
actual_copies=$(grep -E '^[[:space:]]*COPY([[:space:]]|$)' "$df")
[[ "$actual_copies" == "$expected_copies" ]] || die "Dockerfile COPY sequence differs"

final_copy_line=$(grep -Fn 'COPY --from=builder /usr/local/bin/fastly /usr/local/bin/fastly' "$df" | cut -d: -f1)
! tail -n "+$final_copy_line" "$df" | grep -Eq '^[[:space:]]*RUN([[:space:]]|$)' ||
  die "a command can replace final-stage installed assets"

grep -Fq 'sccache_sha="1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b"' "$df" ||
  die "sccache checksum is not exact"
grep -Fq 'asset="sccache-v0.10.0-x86_64-unknown-linux-musl.tar.gz"' "$df" ||
  die "sccache asset is not exact"
# shellcheck disable=SC2016 # Match the literal Dockerfile shell variables.
grep -Fq 'curl --fail --location --silent --show-error "$base/$asset.sha256"' "$df" ||
  die "sccache checksum companion is not downloaded"
grep -Fq '3ba3d8a739b7a88d0a612825a9755d735efb87a9b02ea67e53a11b96d178d500' \
  "$context/.github/actions/deploy-fastly/versions.json" || die "Fastly checksum is not exact"

for required in \
  'COPY --from=builder /usr/local/bin/fastly /usr/local/bin/fastly' \
  'COPY --from=builder /usr/local/bin/sccache /usr/local/bin/sccache' \
  'COPY --from=builder /build/.github/tools/edgezero-provenance-validator/target/release/edgezero-provenance-validator /usr/local/bin/edgezero-provenance-validator' \
  'COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/components /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/components' \
  'COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/manifest-rust-std-wasm32-wasip1 /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/manifest-rust-std-wasm32-wasip1' \
  'COPY --from=builder /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/multirust-config.toml /usr/local/rustup/toolchains/1.95.0-x86_64-unknown-linux-gnu/lib/rustlib/multirust-config.toml' \
  'COPY .github/docker/build-app-cli/provenance.schema.json /usr/local/share/edgezero/provenance.schema.json' \
  'COPY .github/docker/build-app-cli/fixtures/provenance /usr/local/share/edgezero/provenance-fixtures' \
  'COPY .github/docker/build-app-cli/fixtures/gnu-smoke.rs /usr/local/share/edgezero/gnu-smoke.rs' \
  'COPY .github/docker/build-app-cli/fixtures/wasm-smoke.rs /usr/local/share/edgezero/wasm-smoke.rs' \
  'COPY .github/docker/build-app-cli/verify-toolchain.sh /usr/local/bin/verify-toolchain' \
  'USER 1001:1001' \
  'ENTRYPOINT ["/usr/bin/env"]'; do
  [[ "$(grep -Fxc "$required" "$df")" == "1" ]] || die "missing exact Dockerfile contract: $required"
done

user_line=$(grep -Fn 'USER 1001:1001' "$df" | cut -d: -f1)
! tail -n "+$((user_line + 1))" "$df" | grep -Eq '^[[:space:]]*(RUN|COPY|ADD)([[:space:]]|$)' ||
  die "installed image content are replaced after final USER"

jq -e '
  .fastly.version == "15.1.0" and
  .fastly.linux_amd64.url == "https://github.com/fastly/cli/releases/download/v15.1.0/fastly_v15.1.0_linux-amd64.tar.gz" and
  .fastly.linux_amd64.sha256 == "3ba3d8a739b7a88d0a612825a9755d735efb87a9b02ea67e53a11b96d178d500" and
  .rust_target == "wasm32-wasip1"
' "$context/.github/actions/deploy-fastly/versions.json" >/dev/null || die "Fastly metadata differs"
