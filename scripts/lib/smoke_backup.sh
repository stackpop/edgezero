# shellcheck shell=bash
# Sourceable backup/restore helpers shared by the smoke scripts.
#
# Backups are FAIL-CLOSED: any failure to capture an original aborts the
# caller (exit 1) rather than recording a partial or empty backup that a
# later restore could write back OVER the operator's file. The caller must
# record backups (via `backup_in_tree`) BEFORE it arms its restore trap and
# BEFORE it mutates anything, so a failed backup exits with the tree still
# pristine and the trap never runs.
#
# Restores never discard a backup they could not apply, so a mid-restore
# failure stays recoverable by hand.
#
# Entry format: "orig::existed::backup" (existed = 0|1; backup empty when 0).
BACKUPS=()

# Record a fail-closed backup of a FILE or DIRECTORY the smoke is about to
# mutate. Records the original-existence flag separately so an absent file
# and a present-but-empty file are distinguishable on restore.
backup_in_tree() {
  local orig="$1"
  local back="" existed=0
  if [ -e "$orig" ] || [ -L "$orig" ]; then
    existed=1
    if [ -d "$orig" ]; then
      back=$(mktemp -d) || {
        printf 'FATAL: mktemp -d failed backing up %s\n' "$orig" >&2
        exit 1
      }
      # No `|| true`: a suppressed partial copy would later restore
      # incomplete data over the operator's directory.
      if ! cp -a "$orig/." "$back/"; then
        printf 'FATAL: failed to back up directory %s; aborting before any mutation to avoid data loss\n' "$orig" >&2
        rm -rf "$back"
        exit 1
      fi
    else
      back=$(mktemp) || {
        printf 'FATAL: mktemp failed backing up %s\n' "$orig" >&2
        exit 1
      }
      if ! cp -p "$orig" "$back"; then
        printf 'FATAL: failed to back up %s; aborting before any mutation to avoid data loss\n' "$orig" >&2
        rm -f "$back"
        exit 1
      fi
    fi
  fi
  # Only recorded AFTER a successful capture -- a failed backup exits above
  # and never lands in BACKUPS, so restore can never write a bad backup.
  BACKUPS+=("${orig}::${existed}::${back}")
}

# Restore every recorded backup in REVERSE order (a nested path restores
# after its parent). Uses the recorded existed-flag -- NOT the backup's
# size -- so a pre-existing EMPTY file/dir is preserved, not deleted.
#
# Fail-closed: the restore is STAGED into a sibling temp path and only then
# ATOMICALLY swapped into place, so a copy failure NEVER leaves the live
# path deleted. On any failure the live state is left intact (or rolled
# back) and a non-zero status is returned so the caller can surface it --
# the earlier version deleted the directory first and returned success even
# when the copy failed.
restore_backups() {
  local i entry orig existed back rc=0
  for (( i=${#BACKUPS[@]}-1; i>=0; i-- )); do
    entry="${BACKUPS[$i]}"
    [ -z "$entry" ] && continue
    orig="${entry%%::*}"
    back="${entry##*::}"
    existed="${entry#*::}"; existed="${existed%%::*}"
    if [ "$existed" != "1" ]; then
      # Original was ABSENT: remove whatever the smoke created, discard
      # any placeholder backup.
      rm -rf "$orig" 2>/dev/null || true
      if [ -n "$back" ]; then
        rm -rf "$back" 2>/dev/null || true
      fi
      continue
    fi

    # Stage the restore in a SIBLING temp (same filesystem, so the final
    # swap is an atomic rename), proving the copy succeeds BEFORE the live
    # path is touched.
    local staged
    if [ -d "$back" ]; then
      staged=$(mktemp -d "${orig}.restore.XXXXXX" 2>/dev/null) || {
        printf '  FAIL  restore_backups: mktemp -d failed staging %s\n' "$orig" >&2
        rc=1
        continue
      }
      if ! cp -a "$back/." "$staged/"; then
        rm -rf "$staged" 2>/dev/null || true
        printf '  FAIL  restore_backups: could not stage restore of %s; live state left intact\n' "$orig" >&2
        rc=1
        continue
      fi
    else
      staged=$(mktemp "${orig}.restore.XXXXXX" 2>/dev/null) || {
        printf '  FAIL  restore_backups: mktemp failed staging %s\n' "$orig" >&2
        rc=1
        continue
      }
      if ! cp -pf "$back" "$staged"; then
        rm -f "$staged" 2>/dev/null || true
        printf '  FAIL  restore_backups: could not stage restore of %s; live state left intact\n' "$orig" >&2
        rc=1
        continue
      fi
    fi

    # Atomic swap: move the live path aside, move the staged restore in,
    # drop the aside copy. Roll back on failure so the original survives.
    local aside="${orig}.prev.$$"
    if { [ -e "$orig" ] || [ -L "$orig" ]; } && ! mv "$orig" "$aside" 2>/dev/null; then
      rm -rf "$staged" 2>/dev/null || true
      printf '  FAIL  restore_backups: could not move current %s aside; live state left intact\n' "$orig" >&2
      rc=1
      continue
    fi
    if mv "$staged" "$orig" 2>/dev/null; then
      rm -rf "$aside" "$back" 2>/dev/null || true
    else
      # Swap failed: roll the live path back from the aside copy.
      if [ -e "$aside" ] || [ -L "$aside" ]; then
        mv "$aside" "$orig" 2>/dev/null || true
      fi
      rm -rf "$staged" 2>/dev/null || true
      printf '  FAIL  restore_backups: could not swap restored %s into place; original left intact\n' "$orig" >&2
      rc=1
    fi
  done
  BACKUPS=()
  return "$rc"
}
