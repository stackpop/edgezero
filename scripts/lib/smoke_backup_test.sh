#!/usr/bin/env bash
# Unit tests for scripts/lib/smoke_backup.sh. Runs without any live
# provider -- exercises exact preservation AND the fail-closed path where a
# failing `cp` must abort WITHOUT touching the operator's original.
#
# NOTE: deliberately NOT `set -e` -- the tests drive failure paths on
# purpose and assert on their exit codes.
set -uo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/lib/smoke_backup.sh
. "$DIR/smoke_backup.sh"

pass=0
fail=0
check() {
  local label="$1" got="$2" want="$3"
  if [ "$got" = "$want" ]; then
    pass=$((pass + 1))
  else
    printf 'FAIL  %s: expected %q, got %q\n' "$label" "$want" "$got" >&2
    fail=$((fail + 1))
  fi
}

work=$(mktemp -d)

# 1. A mutated file is restored to its original content.
printf 'original\n' > "$work/f"
BACKUPS=(); backup_in_tree "$work/f"
printf 'mutated\n' > "$work/f"
restore_backups
check "content-restore" "$(cat "$work/f")" "original"

# 2. A present-but-EMPTY file is preserved (not deleted).
: > "$work/empty"
BACKUPS=(); backup_in_tree "$work/empty"
printf 'junk\n' > "$work/empty"
restore_backups
check "empty-still-exists" "$([ -f "$work/empty" ] && echo yes || echo no)" "yes"
check "empty-still-empty" "$(wc -c < "$work/empty" | tr -d ' ')" "0"

# 3. An originally-ABSENT file the smoke creates is removed on restore.
BACKUPS=(); backup_in_tree "$work/absent"
printf 'created\n' > "$work/absent"
restore_backups
check "absent-removed" "$([ -e "$work/absent" ] && echo yes || echo no)" "no"

# 4. A directory is restored exactly: mutated files reverted, added files
#    dropped, deleted files restored.
mkdir -p "$work/d/sub"
printf 'a\n' > "$work/d/a"
printf 'b\n' > "$work/d/sub/b"
BACKUPS=(); backup_in_tree "$work/d"
printf 'mutated\n' > "$work/d/a"
rm "$work/d/sub/b"
printf 'extra\n' > "$work/d/added"
restore_backups
check "dir-file-reverted" "$(cat "$work/d/a")" "a"
check "dir-deleted-restored" "$(cat "$work/d/sub/b")" "b"
check "dir-added-dropped" "$([ -e "$work/d/added" ] && echo yes || echo no)" "no"

# 5. FAIL-CLOSED: a failing `cp` must abort (exit 1) and leave the original
#    UNTOUCHED -- never record an empty/partial backup that restore could
#    later write back over the file.
printf 'precious\n' > "$work/g"
(
  # Shadow `cp` so the capture fails; the subshell isolates the exit 1.
  cp() { return 1; }
  BACKUPS=()
  backup_in_tree "$work/g"
) 2>/dev/null
rc=$?
check "fail-closed-exit-nonzero" "$rc" "1"
check "fail-closed-original-intact" "$(cat "$work/g")" "precious"

rm -rf "$work"

printf 'smoke_backup_test: %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
