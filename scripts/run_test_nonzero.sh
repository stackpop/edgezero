#!/bin/sh

set -eu

ignored=
if [ "${1:-}" = "--ignored" ]; then
    ignored=--ignored
    shift
fi

if [ "$#" -lt 2 ]; then
    echo "usage: $0 [--ignored] <sentinel> <command...>" >&2
    exit 2
fi

sentinel=$1
shift
listing=$(mktemp "${TMPDIR:-/tmp}/edgezero-tests.XXXXXX")
trap 'rm -f "$listing"' EXIT HUP INT TERM

if [ -n "$ignored" ]; then
    "$@" -- --ignored --list >"$listing"
else
    "$@" -- --list >"$listing"
fi

awk -v sentinel="$sentinel" '
    /: test$/ {
        count += 1
        name = $0
        sub(/: test$/, "", name)
        if (name == sentinel || name ~ ("::" sentinel "$")) {
            found = 1
        }
    }
    END {
        if (count == 0) {
            print "selected test suite contains zero tests" > "/dev/stderr"
            exit 1
        }
        if (!found) {
            print "required sentinel test not listed: " sentinel > "/dev/stderr"
            exit 1
        }
    }
' "$listing"

if [ -n "$ignored" ]; then
    "$@" -- --ignored
else
    "$@"
fi
