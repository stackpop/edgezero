#!/usr/bin/env bash
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ASSERT="$DIR/../../../docker/build-app-cli/assert-build-container-completion.sh"
WORK=$(mktemp -d)
WORK=$(cd -- "$WORK" && pwd -P)
trap 'rm -rf "$WORK"' EXIT
MARKER="$WORK/completion"

pass=0
fail=0

should_pass() {
  local description=$1 kind=$2 value=$3
  printf '%s' "$value" >"$MARKER"
  if bash "$ASSERT" --file "$MARKER" --kind "$kind" >"$WORK/stdout" 2>"$WORK/stderr" &&
    [[ ! -s "$WORK/stdout" && ! -s "$WORK/stderr" ]]; then
    printf '  \033[32mok\033[0m   %s\n' "$description"
    pass=$((pass + 1))
  else
    cat "$WORK/stdout" "$WORK/stderr" >&2
    printf '  \033[31mFAIL\033[0m %s\n' "$description" >&2
    fail=$((fail + 1))
  fi
}

should_fail() {
  local description=$1 kind=$2 value=$3
  rm -f "$MARKER"
  [[ "$value" == absent ]] || printf '%s' "$value" >"$MARKER"
  if bash "$ASSERT" --file "$MARKER" --kind "$kind" >"$WORK/stdout" 2>"$WORK/stderr"; then
    printf '  \033[31mFAIL\033[0m %s\n' "$description" >&2
    fail=$((fail + 1))
  elif [[ -s "$WORK/stdout" ]]; then
    cat "$WORK/stdout" >&2
    printf '  \033[31mFAIL\033[0m %s emitted stdout\n' "$description" >&2
    fail=$((fail + 1))
  else
    printf '  \033[32mok\033[0m   %s\n' "$description"
    pass=$((pass + 1))
  fi
}

echo '== build-container completion marker =='

for kind in local pin; do
  printf -v marker 'kind=%s\nmode=ordinary\nbranch=relevant\n' "$kind"
  should_pass "$kind ordinary relevant marker is exact" "$kind" "$marker"
  printf -v marker 'kind=%s\nmode=ordinary\nbranch=not-applicable\n' "$kind"
  should_pass "$kind ordinary not-applicable marker is exact" "$kind" "$marker"
  printf -v marker 'kind=%s\nmode=gate-update\nbranch=gate-update\n' "$kind"
  should_pass "$kind gate-update marker is exact" "$kind" "$marker"
  printf -v marker 'kind=%s\nmode=gate-rollback\nbranch=gate-rollback\n' "$kind"
  should_pass "$kind gate-rollback marker is exact" "$kind" "$marker"
done

should_fail 'missing marker fails' local absent
should_fail 'empty marker fails' local ''
should_fail 'wrong kind fails' local $'kind=pin\nmode=ordinary\nbranch=relevant\n'
should_fail 'unknown mode fails' local $'kind=local\nmode=other\nbranch=relevant\n'
should_fail 'contradictory ordinary branch fails' local \
  $'kind=local\nmode=ordinary\nbranch=gate-update\n'
should_fail 'contradictory gate branch fails' local \
  $'kind=local\nmode=gate-update\nbranch=relevant\n'
should_fail 'duplicate fields fail' local \
  $'kind=local\nkind=local\nmode=ordinary\nbranch=relevant\n'
should_fail 'extra fields fail' local \
  $'kind=local\nmode=ordinary\nbranch=relevant\nextra=true\n'
should_fail 'missing final LF fails' local \
  $'kind=local\nmode=ordinary\nbranch=relevant'

printf 'kind=local\0\nmode=ordinary\nbranch=relevant\n' >"$MARKER"
if bash "$ASSERT" --file "$MARKER" --kind local >"$WORK/stdout" 2>"$WORK/stderr"; then
  printf '  \033[31mFAIL\033[0m NUL-bearing marker fails\n' >&2
  fail=$((fail + 1))
else
  printf '  \033[32mok\033[0m   NUL-bearing marker fails\n'
  pass=$((pass + 1))
fi

printf 'kind=local\nmode=ordinary\nbranch=relevant\n' >"$WORK/target"
rm -f "$MARKER"
ln -s "$WORK/target" "$MARKER"
if bash "$ASSERT" --file "$MARKER" --kind local >"$WORK/stdout" 2>"$WORK/stderr"; then
  printf '  \033[31mFAIL\033[0m symlink marker fails\n' >&2
  fail=$((fail + 1))
else
  printf '  \033[32mok\033[0m   symlink marker fails\n'
  pass=$((pass + 1))
fi

if ((fail)); then
  printf '\n%d passed, %d failed\n' "$pass" "$fail" >&2
  exit 1
fi
printf '\n%d passed, %d failed\n' "$pass" "$fail"
