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
# size -- so a pre-existing EMPTY file/dir is preserved, not deleted. A
# backup that fails to apply is KEPT (and warned about) rather than
# discarded, so the original stays recoverable.
restore_backups() {
  local i entry orig existed back
  for (( i=${#BACKUPS[@]}-1; i>=0; i-- )); do
    entry="${BACKUPS[$i]}"
    [ -z "$entry" ] && continue
    orig="${entry%%::*}"
    back="${entry##*::}"
    existed="${entry#*::}"; existed="${existed%%::*}"
    if [ "$existed" = "1" ]; then
      if [ -d "$back" ]; then
        # Clear whatever the smoke left (to drop files it ADDED) then
        # restore the captured tree.
        rm -rf "$orig" 2>/dev/null || true
        mkdir -p "$orig"
        if cp -a "$back/." "$orig/"; then
          rm -rf "$back"
        else
          printf '  WARN  restore_backups: failed to restore directory %s from %s; backup kept for manual recovery\n' "$orig" "$back" >&2
        fi
      else
        # File (possibly empty). `cp -pf` overwrites whatever the smoke
        # left; the backup is discarded ONLY on success.
        if cp -pf "$back" "$orig"; then
          rm -f "$back"
        else
          printf '  WARN  restore_backups: failed to restore %s from %s; backup kept for manual recovery\n' "$orig" "$back" >&2
        fi
      fi
    else
      # Original was ABSENT: remove whatever the smoke created, discard
      # any placeholder backup.
      rm -rf "$orig" 2>/dev/null || true
      if [ -n "$back" ]; then
        rm -rf "$back" 2>/dev/null || true
      fi
    fi
  done
  BACKUPS=()
}
