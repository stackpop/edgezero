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
reset_backups; backup_in_tree "$work/f"
printf 'mutated\n' > "$work/f"
restore_backups
check "content-restore" "$(cat "$work/f")" "original"

# 2. A present-but-EMPTY file is preserved (not deleted).
: > "$work/empty"
reset_backups; backup_in_tree "$work/empty"
printf 'junk\n' > "$work/empty"
restore_backups
check "empty-still-exists" "$([ -f "$work/empty" ] && echo yes || echo no)" "yes"
check "empty-still-empty" "$(wc -c < "$work/empty" | tr -d ' ')" "0"

# 3. An originally-ABSENT file the smoke creates is removed on restore.
reset_backups; backup_in_tree "$work/absent"
printf 'created\n' > "$work/absent"
restore_backups
check "absent-removed" "$([ -e "$work/absent" ] && echo yes || echo no)" "no"

# 4. A directory is restored exactly: mutated files reverted, added files
#    dropped, deleted files restored.
mkdir -p "$work/d/sub"
printf 'a\n' > "$work/d/a"
printf 'b\n' > "$work/d/sub/b"
reset_backups; backup_in_tree "$work/d"
printf 'mutated\n' > "$work/d/a"
rm "$work/d/sub/b"
printf 'extra\n' > "$work/d/added"
restore_backups
check "dir-file-reverted" "$(cat "$work/d/a")" "a"
check "dir-deleted-restored" "$(cat "$work/d/sub/b")" "b"
check "dir-added-dropped" "$([ -e "$work/d/added" ] && echo yes || echo no)" "no"

# 5. FAIL-CLOSED backup: a failing `cp` must abort (exit 1) and leave the
#    original UNTOUCHED -- never record an empty/partial backup that restore
#    could later write back over the file.
printf 'precious\n' > "$work/g"
(
  # Shadow `cp` so the capture fails; the subshell isolates the exit 1.
  cp() { return 1; }
  reset_backups
  backup_in_tree "$work/g"
) 2>/dev/null
rc=$?
check "fail-closed-exit-nonzero" "$rc" "1"
check "fail-closed-original-intact" "$(cat "$work/g")" "precious"

# 6. FAIL-CLOSED restore: a failing restore copy must NOT delete the live
#    directory, must RETURN NON-ZERO, and must RETAIN the failed entry's
#    metadata (backup path) for recovery. Runs in the PARENT process (not a
#    subshell) so the retained-metadata mutation is actually observable.
mkdir -p "$work/live/sub"
printf 'live-a\n' > "$work/live/a"
reset_backups; backup_in_tree "$work/live"
retained_back="${BK_BACK[0]}"
printf 'mutated\n' > "$work/live/a"
cp() { return 1; }   # shadow cp so restore staging fails
restore_rc=0
restore_backups 2>/dev/null || restore_rc=$?
unset -f cp
check "restore-failure-returns-nonzero" "$restore_rc" "1"
check "restore-failure-keeps-live-dir" "$([ -d "$work/live" ] && echo yes || echo no)" "yes"
check "restore-failure-live-content-intact" "$([ -f "$work/live/a" ] && echo yes || echo no)" "yes"
check "restore-failure-retains-metadata" "${BK_BACK[0]:-}" "$retained_back"
reset_backups

# 8. A path CONTAINING the old `::` delimiter must restore correctly and
#    must NOT truncate to (and delete) an unrelated ancestor path.
mkdir -p "$work/proj::review"
printf 'kept\n' > "$work/proj::review/f"
mkdir -p "$work/proj"
printf 'unrelated\n' > "$work/proj/keep"
reset_backups; backup_in_tree "$work/proj::review/f"
printf 'mutated\n' > "$work/proj::review/f"
restore_backups
check "delimiter-path-restored" "$(cat "$work/proj::review/f")" "kept"
check "delimiter-path-no-collateral" "$([ -f "$work/proj/keep" ] && echo yes || echo no)" "yes"

# 7. SYMLINK capture is refused (exit 1) and the link + its target are left
#    untouched, so `cp` can't follow the link and lose its identity.
printf 'target-content\n' > "$work/target"
ln -s "$work/target" "$work/link"
(
  reset_backups
  backup_in_tree "$work/link"
) 2>/dev/null
rc=$?
check "symlink-capture-refused" "$rc" "1"
check "symlink-still-a-link" "$([ -L "$work/link" ] && echo yes || echo no)" "yes"
check "symlink-target-untouched" "$(cat "$work/target")" "target-content"

# 9. smoke_stop_server with an EMPTY pid (a pre-launch failure) must be a
#    no-op: it must NOT hunt/kill port holders, or it would SIGKILL an
#    unrelated service that happens to hold the configured port.
lsof_calls=0
kill_calls=0
lsof() { lsof_calls=$((lsof_calls + 1)); return 0; }
kill() { kill_calls=$((kill_calls + 1)); return 0; }
pkill() { kill_calls=$((kill_calls + 1)); return 0; }
smoke_stop_server "" "65535"
stop_rc=$?
unset -f lsof kill pkill
check "empty-pid-stop-returns-zero" "$stop_rc" "0"
check "empty-pid-stop-no-port-kill" "$kill_calls" "0"
check "empty-pid-stop-no-lsof" "$lsof_calls" "0"

rm -rf "$work"

printf 'smoke_backup_test: %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
