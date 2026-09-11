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
# Backup metadata is kept in three PARALLEL arrays keyed by index, NOT a
# `::`-joined string -- an operator checkout path can legitimately contain
# `::` (e.g. `/tmp/project::review`), and splitting on `::` truncated the
# path and fed the wrong value to `rm -rf`.
BK_ORIG=()     # the in-tree path that was backed up
BK_EXISTED=()  # "1" if it existed at capture, "0" if absent
BK_BACK=()     # the temp path holding the captured copy ("" when absent)

# Clear all recorded backups. Used by tests between cases.
reset_backups() {
  BK_ORIG=()
  BK_EXISTED=()
  BK_BACK=()
}

# Is PID $1 a descendant of (or equal to) PID $2? Walks the parent-pid
# chain up to init, bounded so a cycle/oddity can't loop forever. Used to
# prove a port holder is OURS before killing it.
smoke_pid_is_descendant() {
  local cur="$1" ancestor="$2" guard=0
  while [ -n "$cur" ] && [ "$cur" != "0" ] && [ "$cur" != "1" ] && [ "$guard" -lt 64 ]; do
    if [ "$cur" = "$ancestor" ]; then
      return 0
    fi
    cur=$(ps -o ppid= -p "$cur" 2>/dev/null | tr -d ' ')
    guard=$((guard + 1))
  done
  [ "$cur" = "$ancestor" ]
}

# Stop the smoke's server and its DESCENDANTS, then wait until the port is
# actually free -- BEFORE restoration. The wrapper PID's direct children
# aren't enough: `wrangler` spawns `workerd`, `spin` forks, etc., and a
# surviving descendant can flush state AFTER the backup was restored,
# re-corrupting the developer's tree. Args: <wrapper-pid> [port].
smoke_stop_server() {
  local pid="$1"
  local port="${2:-}"
  # CRUCIAL: only touch the port when WE actually started a server (a
  # non-empty wrapper pid). A pre-launch failure calls cleanup with an
  # empty pid; port-killing then would SIGKILL whatever unrelated service
  # already holds the port -- data loss in someone else's process.
  if [ -z "$pid" ]; then
    return 0
  fi
  # Capture the port holders that are OUR DESCENDANTS *before* tearing the
  # tree down. Afterwards a re-parented grand-child (adopted by init) can no
  # longer be attributed to us -- and if our server died before binding and
  # an UNRELATED process then grabbed the port, that process is not our
  # descendant and must never be killed. We only ever kill what we can prove
  # is ours.
  local owned_holders=""
  if [ -n "$port" ] && command -v lsof >/dev/null 2>&1; then
    local holder
    for holder in $(lsof -ti ":${port}" 2>/dev/null || true); do
      if smoke_pid_is_descendant "$holder" "$pid"; then
        owned_holders="$owned_holders $holder"
      fi
    done
  fi
  pkill -TERM -P "$pid" 2>/dev/null || true
  kill -TERM "$pid" 2>/dev/null || true
  local waited=0
  while [ "$waited" -lt 5 ] && kill -0 "$pid" 2>/dev/null; do
    sleep 1
    waited=$((waited + 1))
  done
  pkill -KILL -P "$pid" 2>/dev/null || true
  kill -KILL "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  # KILL the OWNED port holders captured above (workerd/spin grand-children
  # that `pkill -P` didn't cover) -- never an unrelated holder.
  if [ -n "$owned_holders" ]; then
    # shellcheck disable=SC2086
    kill -KILL $owned_holders 2>/dev/null || true
  fi
  # Wait for the port to clear as a courtesy; do NOT kill whoever holds it
  # now (it may be an unrelated process that grabbed the freed port).
  if [ -n "$port" ] && command -v lsof >/dev/null 2>&1; then
    waited=0
    while [ "$waited" -lt 5 ] && lsof -ti ":${port}" >/dev/null 2>&1; do
      sleep 1
      waited=$((waited + 1))
    done
  fi
}

# Refuse to launch when the target port is ALREADY held by an unrelated
# process: exit non-zero rather than proceed (and later SIGKILL that
# process during cleanup). Call BEFORE starting the server. A no-op when
# `lsof` is unavailable. Arg: <port>.
smoke_require_port_free() {
  local port="$1"
  if [ -n "$port" ] && command -v lsof >/dev/null 2>&1 && lsof -ti ":${port}" >/dev/null 2>&1; then
    printf 'FATAL: port %s is already in use by another process; refusing to launch (the smoke will not kill a service it did not start). Free the port and re-run.\n' "$port" >&2
    exit 1
  fi
}

# Record a fail-closed backup of a FILE or DIRECTORY the smoke is about to
# mutate. Records the original-existence flag separately so an absent file
# and a present-but-empty file are distinguishable on restore.
backup_in_tree() {
  local orig="$1"
  local back="" existed=0
  # Refuse a SYMLINK (file or dir): these are EdgeZero-owned local files
  # that provision/push never create through a link, so one appearing here
  # is anomalous. Following it would (a) capture the link TARGET's content
  # via `cp`, losing link identity, and (b) leave the target mutated while
  # restore writes back a regular object. Fail closed -- matching the
  # provision-side `reject_symlinked_target` policy -- BEFORE any mutation.
  if [ -L "$orig" ]; then
    printf 'FATAL: `%s` is a symlink; refusing to back it up (EdgeZero-owned local state is never a symlink). Replace it with a regular file/dir before running the smoke.\n' "$orig" >&2
    exit 1
  fi
  if [ -e "$orig" ]; then
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
  # and never lands in the arrays, so restore can never write a bad backup.
  BK_ORIG+=("$orig")
  BK_EXISTED+=("$existed")
  BK_BACK+=("$back")
}

# Restore ONE recorded backup. Returns 0 on success, 1 on failure (leaving
# the live path intact / rolled back). Staged into a sibling temp then
# atomically swapped so a copy failure never deletes the live path.
restore_one() {
  local orig="$1" existed="$2" back="$3"
  if [ "$existed" != "1" ]; then
    # Original was ABSENT: remove whatever the smoke created, discard the
    # placeholder backup.
    rm -rf "$orig" 2>/dev/null || true
    [ -n "$back" ] && rm -rf "$back" 2>/dev/null || true
    return 0
  fi

  local staged
  if [ -d "$back" ]; then
    staged=$(mktemp -d "${orig}.restore.XXXXXX" 2>/dev/null) || {
      printf '  FAIL  restore: mktemp -d failed staging %s\n' "$orig" >&2
      return 1
    }
    if ! cp -a "$back/." "$staged/"; then
      rm -rf "$staged" 2>/dev/null || true
      printf '  FAIL  restore: could not stage restore of %s; live state left intact\n' "$orig" >&2
      return 1
    fi
  else
    staged=$(mktemp "${orig}.restore.XXXXXX" 2>/dev/null) || {
      printf '  FAIL  restore: mktemp failed staging %s\n' "$orig" >&2
      return 1
    }
    if ! cp -pf "$back" "$staged"; then
      rm -f "$staged" 2>/dev/null || true
      printf '  FAIL  restore: could not stage restore of %s; live state left intact\n' "$orig" >&2
      return 1
    fi
  fi

  # Atomic swap: move the live path aside, move the staged restore in, drop
  # the aside copy. Roll back on failure so the original survives.
  local aside="${orig}.prev.$$"
  if { [ -e "$orig" ] || [ -L "$orig" ]; } && ! mv "$orig" "$aside" 2>/dev/null; then
    rm -rf "$staged" 2>/dev/null || true
    printf '  FAIL  restore: could not move current %s aside; live state left intact\n' "$orig" >&2
    return 1
  fi
  if mv "$staged" "$orig" 2>/dev/null; then
    rm -rf "$aside" "$back" 2>/dev/null || true
    return 0
  fi
  # Swap failed: roll the live path back from the aside copy.
  if [ -e "$aside" ] || [ -L "$aside" ]; then
    mv "$aside" "$orig" 2>/dev/null || true
  fi
  rm -rf "$staged" 2>/dev/null || true
  printf '  FAIL  restore: could not swap restored %s into place; original left intact\n' "$orig" >&2
  return 1
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
  local i rc=0
  # Entries that FAIL to restore are RETAINED (their backup paths kept) so a
  # retry / manual recovery isn't orphaned; successful ones are dropped.
  local -a f_orig=() f_existed=() f_back=()
  for (( i=${#BK_ORIG[@]}-1; i>=0; i-- )); do
    if restore_one "${BK_ORIG[$i]}" "${BK_EXISTED[$i]}" "${BK_BACK[$i]}"; then
      continue
    fi
    rc=1
    f_orig+=("${BK_ORIG[$i]}")
    f_existed+=("${BK_EXISTED[$i]}")
    f_back+=("${BK_BACK[$i]}")
  done
  # Keep only the failed entries (empty-array-safe for bash 3.2 under set -u).
  BK_ORIG=(${f_orig[@]+"${f_orig[@]}"})
  BK_EXISTED=(${f_existed[@]+"${f_existed[@]}"})
  BK_BACK=(${f_back[@]+"${f_back[@]}"})
  return "$rc"
}
