#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

command -v cargo >/dev/null 2>&1 || {
  echo "cargo is required but was not found in PATH" >&2
  exit 1
}

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "cargo-llvm-cov is required. Install with 'cargo install cargo-llvm-cov' and 'rustup component add llvm-tools-preview'." >&2
  exit 1
fi

OUTPUT_DIR="target/coverage"
mkdir -p "$OUTPUT_DIR"

discover_packages() {
  if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required to auto-discover workspace packages. Set EDGEZERO_COVERAGE_PACKAGES to skip discovery." >&2
    exit 1
  fi

  local include_bins=false
  case "${EDGEZERO_COVERAGE_INCLUDE_BINS:-0}" in
    "" | 0 | false | False) ;;
    *) include_bins=true ;;
  esac

  cargo metadata --format-version 1 --no-deps |
    jq -r --argjson include_bins "$include_bins" '
      .workspace_members as $workspace
      | [.packages[]
          | select(.id as $id | $workspace | index($id))
          | select(
              $include_bins
              or any(.targets[]?; any(.kind[]?; . == "lib" or . == "proc-macro"))
            )
          | .name]
      | join(" ")
    '
}

if [ -n "${EDGEZERO_COVERAGE_PACKAGES:-}" ]; then
  PACKAGES="${EDGEZERO_COVERAGE_PACKAGES}"
else
  PACKAGES="$(discover_packages)"
  echo "==> Auto-discovered packages: ${PACKAGES}"
fi

if [ -z "${PACKAGES}" ]; then
  echo "No packages selected for coverage. Set EDGEZERO_COVERAGE_PACKAGES to override." >&2
  exit 1
fi

for pkg in $PACKAGES; do
  echo "==> Coverage for ${pkg}"
  cargo llvm-cov -p "${pkg}" --lcov --output-path "${OUTPUT_DIR}/${pkg}.lcov"

  if command -v genhtml >/dev/null 2>&1; then
    html_dir="${OUTPUT_DIR}/${pkg}-html"
    echo "==> HTML report for ${pkg}"
    genhtml "${OUTPUT_DIR}/${pkg}.lcov" -o "${html_dir}" >/dev/null
  fi
done

echo "Coverage reports saved under ${OUTPUT_DIR}/"
if command -v genhtml >/dev/null 2>&1; then
  echo "HTML reports saved under ${OUTPUT_DIR}/<package>-html/"
else
  echo "Install 'genhtml' (lcov) to generate HTML reports."
fi
