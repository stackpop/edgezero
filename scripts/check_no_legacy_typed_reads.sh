#!/usr/bin/env bash
# check_no_legacy_typed_reads.sh — CI gate per spec §10.2.1
#
# Refuses any usage shape that indicates a legacy per-leaf typed-config read
# (the pattern where each handler calls config_store_default()?.get(...) or
# secret_store.require_str(&cfg.<field>) directly instead of using the
# AppConfig<C> blob-model extractor).
#
# Also refuses nested AppConfig extractors (AppConfig<...AppConfig<...>...>),
# which are illegal per spec §3.3.
#
# Exit 0 — clean tree.
# Exit 1 — at least one violation found; prints <file>:<line>: violation: <pattern>.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VIOLATIONS=0

# ------------------------------------------------------------------
# Pattern 1: legacy per-leaf config read
#   config_store_default()?.get(
# ------------------------------------------------------------------
while IFS= read -r hit; do
    printf '%s: violation: legacy per-leaf config read — use AppConfig<C> extractor instead\n' "${hit}"
    VIOLATIONS=$((VIOLATIONS + 1))
done < <(
    grep -rn --include="*.rs" \
        'config_store_default()?\s*\.\s*get(' \
        "${REPO_ROOT}/crates" \
        "${REPO_ROOT}/examples" \
        2>/dev/null || true
)

# ------------------------------------------------------------------
# Pattern 2: legacy per-leaf secret read
#   secret_store.require_str(&cfg.
# ------------------------------------------------------------------
while IFS= read -r hit; do
    printf '%s: violation: legacy per-leaf secret read — use AppConfig<C> extractor instead\n' "${hit}"
    VIOLATIONS=$((VIOLATIONS + 1))
done < <(
    grep -rn --include="*.rs" \
        'secret_store\.require_str(&cfg\.' \
        "${REPO_ROOT}/crates" \
        "${REPO_ROOT}/examples" \
        2>/dev/null || true
)

# ------------------------------------------------------------------
# Pattern 3: nested AppConfig extractor (illegal per spec §3.3)
#   AppConfig<...AppConfig<...>...>
#
# Excludes the syn-based gate binary that legitimately references
# the forbidden pattern in its violation message + doc-comment.
# That binary IS the precise (AST-level) gate for this property;
# the grep here is the cheap pre-filter for everyone else.
# ------------------------------------------------------------------
while IFS= read -r hit; do
    printf '%s: violation: nested AppConfig extractor — AppConfig<AppConfig<…>> is illegal per spec §3.3\n' "${hit}"
    VIOLATIONS=$((VIOLATIONS + 1))
done < <(
    grep -rn --include="*.rs" \
        --exclude-dir=target \
        --exclude="check_no_nested_app_config.rs" \
        'AppConfig<[^>]*AppConfig<' \
        "${REPO_ROOT}/crates" \
        "${REPO_ROOT}/examples" \
        2>/dev/null || true
)

if [ "${VIOLATIONS}" -gt 0 ]; then
    printf '\n%d violation(s) found. Migrate to the AppConfig<C> blob-model extractor.\n' "${VIOLATIONS}" >&2
    exit 1
fi

printf 'check_no_legacy_typed_reads: OK\n'
exit 0
