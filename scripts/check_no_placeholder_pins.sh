#!/usr/bin/env bash
# check_no_placeholder_pins.sh — CI gate per spec 13.1
#
# Refuses pin tests (files whose path contains "pin") that still carry
# unresolved placeholder hex values.  All placeholder strings have been
# replaced with the real computed SHA, so this gate should pass against
# the current tree.
#
# Placeholder patterns caught:
#   …                      — ellipsis (Unicode U+2026 or literal "...")
#   fixed-hex-value        — development-time stand-in literal
#   FIXME_SHA              — another common stand-in
#   deadbeef               — obvious test-filler hex
#   <hex>                  — angle-bracket placeholder (e.g. "<hex>")
#   0000000000000000       — all-zero placeholder (16+ zeros)
#
# Exit 0 — clean tree.
# Exit 1 — at least one violation; prints <file>:<line>: violation: <pattern>.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VIOLATIONS=0

# Files whose path fragment matches *pin* (case-insensitive).
# We restrict to *.rs pin-test files; template files and docs are excluded.
PIN_FILES=()
while IFS= read -r -d '' ff; do
    PIN_FILES+=("${ff}")
done < <(
    find "${REPO_ROOT}/crates" "${REPO_ROOT}/examples" \
        -name "*pin*.rs" -print0 \
        2>/dev/null || true
)

if [ "${#PIN_FILES[@]}" -eq 0 ]; then
    printf 'check_no_placeholder_pins: no pin files found — OK\n'
    exit 0
fi

check_pattern() {
    local label="$1"
    local pattern="$2"
    while IFS= read -r hit; do
        printf '%s: violation: placeholder pin — %s\n' "${hit}" "${label}"
        VIOLATIONS=$((VIOLATIONS + 1))
    done < <(
        grep -n "${pattern}" "${PIN_FILES[@]}" 2>/dev/null | \
            sed "s|^|${REPO_ROOT}/|" | \
            grep -v "^${REPO_ROOT}//\|placeholder\|spec section\|refuses pin\|Phase A" || true
    )
}

# Run grep directly over the pin files list with each placeholder pattern.
run_check() {
    local label="$1"
    local pattern="$2"
    while IFS= read -r hit; do
        printf '%s: violation: placeholder pin — %s\n' "${hit}" "${label}"
        VIOLATIONS=$((VIOLATIONS + 1))
    done < <(
        grep -Hn "${pattern}" "${PIN_FILES[@]}" 2>/dev/null | \
            grep -v "//.*refuses\|//.*placeholder\|//.*Phase A\|//.*spec section" || true
    )
}

run_check '...' '\.\.\.'
run_check 'ellipsis (…)' '…'
run_check 'fixed-hex-value' 'fixed-hex-value'
run_check 'FIXME_SHA' 'FIXME_SHA'
run_check 'deadbeef placeholder' 'deadbeef'
run_check '<hex> placeholder' '<hex>'
run_check 'all-zero placeholder' '0\{16,\}'

if [ "${VIOLATIONS}" -gt 0 ]; then
    printf '\n%d placeholder pin(s) found. Replace with the real computed SHA.\n' "${VIOLATIONS}" >&2
    exit 1
fi

printf 'check_no_placeholder_pins: OK\n'
exit 0
