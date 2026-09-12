#!/usr/bin/env bash
set -euo pipefail

export LC_ALL=C
export BASH_ENV=
export ENV=

usage() {
  printf 'usage: assert-build-container-completion.sh --file <absolute-path> --kind <local|pin>\n' >&2
  exit 2
}

die() {
  printf '::error::%s\n' "$*" >&2
  exit 1
}

FILE=
KIND=
while (($#)); do
  case "$1" in
    --file)
      if (($# < 2)) || [[ -z "$2" || -n "$FILE" ]]; then usage; fi
      FILE=$2
      shift 2
      ;;
    --kind)
      if (($# < 2)) || [[ -z "$2" || -n "$KIND" ]]; then usage; fi
      KIND=$2
      shift 2
      ;;
    *) usage ;;
  esac
done

[[ -n "$FILE" && -n "$KIND" ]] || usage
[[ "$KIND" == local || "$KIND" == pin ]] || usage
[[ "$FILE" == /* && -f "$FILE" && ! -L "$FILE" ]] ||
  die "completion marker must be an absolute, regular non-symlink file"
CANONICAL=$(cd -- "$(dirname -- "$FILE")" && pwd -P)/$(basename -- "$FILE") ||
  die "cannot resolve completion marker"
[[ "$CANONICAL" == "$FILE" ]] || die "completion marker path must already be canonical"
SIZE=$(wc -c <"$FILE" | tr -d '[:space:]') || die "cannot size completion marker"
[[ "$SIZE" =~ ^[0-9]+$ && "$SIZE" -gt 0 && "$SIZE" -le 128 ]] ||
  die "completion marker is empty or oversized"

if cmp -s <(printf 'kind=%s\nmode=ordinary\nbranch=relevant\n' "$KIND") "$FILE" ||
  cmp -s <(printf 'kind=%s\nmode=ordinary\nbranch=not-applicable\n' "$KIND") "$FILE" ||
  cmp -s <(printf 'kind=%s\nmode=gate-update\nbranch=gate-update\n' "$KIND") "$FILE" ||
  cmp -s <(printf 'kind=%s\nmode=gate-rollback\nbranch=gate-rollback\n' "$KIND") "$FILE"; then
  :
else
  die "completion marker is missing, duplicated, malformed, or contradictory"
fi
