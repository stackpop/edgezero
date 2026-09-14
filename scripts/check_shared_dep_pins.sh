#!/usr/bin/env bash
# check_shared_dep_pins.sh — keep every `validator` pin on one major.
#
# `edgezero-core` re-exports the `Validate` derive, so any crate that
# derives `AppConfig` links against the exact `validator` the core crate
# resolved. A crate that pins a different major gets a second, unrelated
# `Validate` trait and fails with:
#
#   error[E0277]: the trait bound `X: validator::traits::Validate` is not satisfied
#
# The pin lives in four places that cargo cannot reconcile for us: this
# workspace, the excluded `examples/app-demo` workspace, the scaffold
# generator's seed list, and the deploy smoke-test fixture. Only the
# first two share a lockfile, so the others drift silently until a
# deploy job fails.
#
# Exit 0 — every pin agrees.
# Exit 1 — prints each disagreeing file and its version.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Extract validator's version from a file. Accepts the table form with
# `version` in any key position (`validator = { features = [..], version
# = "X" }`), the bare form (`validator = "X"`), and the escaped spelling
# the scaffold seeds it as. Anchored to an assignment at line start so a
# commented-out or prose mention cannot shadow the real pin, and kept
# deliberately in step with `extract_version` in
# crates/edgezero-cli/src/generator.rs -- the two parsers describe the
# same grammar, and a difference between them shows up as this gate
# reporting a missing pin for one that is merely reformatted.
# Prints nothing when there is no match; callers treat empty as missing.
pin_in() {
  grep -hoE '^[[:space:]]*(\\?")?validator[[:space:]]*=[^#]*' "$1" 2>/dev/null |
    head -1 |
    grep -oE '[0-9]+(\.[0-9]+)*' |
    head -1 || true
}

WANT="$(pin_in Cargo.toml)"
if [[ -z "$WANT" ]]; then
  echo "check_shared_dep_pins: no validator pin in root Cargo.toml" >&2
  exit 1
fi

VIOLATIONS=0
for file in \
  examples/app-demo/Cargo.toml \
  .github/actions/deploy-core/tests/make-smoke-fixture.sh; do
  got="$(pin_in "$file")"
  if [[ -z "$got" ]]; then
    echo "$file: violation: no validator pin found (expected $WANT)"
    VIOLATIONS=$((VIOLATIONS + 1))
  elif [[ "$got" != "$WANT" ]]; then
    echo "$file: violation: validator $got, root Cargo.toml has $WANT"
    VIOLATIONS=$((VIOLATIONS + 1))
  fi
done

# The scaffold generator seeds the pin as a Rust string literal, and its
# own unit test compares it against the root manifest; check it here too
# so a mismatch is caught by the same gate as the rest. `pin_in` already
# understands the quoted-literal spelling, so it is reused rather than
# re-implemented -- a second parser here drifted from this one once.
gen="crates/edgezero-cli/src/generator.rs"
got="$(pin_in "$gen")"
if [[ -z "$got" ]]; then
  echo "$gen: violation: no validator seed found (expected $WANT)"
  VIOLATIONS=$((VIOLATIONS + 1))
elif [[ "$got" != "$WANT" ]]; then
  echo "$gen: violation: seeds validator $got, root Cargo.toml has $WANT"
  VIOLATIONS=$((VIOLATIONS + 1))
fi

if ((VIOLATIONS > 0)); then
  echo "check_shared_dep_pins: $VIOLATIONS pin(s) out of sync" >&2
  exit 1
fi
echo "check_shared_dep_pins: all validator pins agree ($WANT)"
