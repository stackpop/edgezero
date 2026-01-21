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
  local python_bin
  python_bin="$(command -v python3 || command -v python || true)"
  if [ -z "${python_bin}" ]; then
    echo "python3 is required to auto-discover workspace packages. Set EDGEZERO_COVERAGE_PACKAGES to skip discovery." >&2
    exit 1
  fi

  EDGEZERO_COVERAGE_INCLUDE_BINS="${EDGEZERO_COVERAGE_INCLUDE_BINS:-0}" \
    cargo metadata --format-version 1 --no-deps | "${python_bin}" -c '
import json
import os
import sys

data = json.load(sys.stdin)
workspace = set(data.get("workspace_members", []))
include_bins = os.environ.get("EDGEZERO_COVERAGE_INCLUDE_BINS", "0") not in ("0", "", "false", "False")

packages = []
for pkg in data.get("packages", []):
    if pkg.get("id") not in workspace:
        continue
    if include_bins:
        packages.append(pkg.get("name"))
        continue
    targets = pkg.get("targets", [])
    has_lib = any(
        "lib" in target.get("kind", []) or "proc-macro" in target.get("kind", [])
        for target in targets
    )
    if has_lib:
        packages.append(pkg.get("name"))

print(" ".join(packages))
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
